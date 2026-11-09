//! Connections imported from TablePlus, from either of the two places it keeps them:
//!
//! - **Its own storage**: `Data/Connections.plist` and `Data/ConnectionGroups.plist` in
//!   TablePlus's folder, a property list on Windows as on macOS. TablePlus keeps passwords and
//!   SSH private keys out of it: on Windows they are in its own encrypted store (`.tpmaster`,
//!   its key protected by the system), which SpelaionDB does not read, so passwords are asked
//!   when connecting and the preview says so. For an SSH key it keeps only the file's name.
//! - **Its export file** (Export Connections → Export all…, `.tableplusconnection`): the same
//!   settings as JSON, groups holding their connections, encrypted with the password the user
//!   chose when exporting, in RNCryptor's documented v3 format (PBKDF2-SHA1 keys, AES-256-CBC,
//!   HMAC-SHA256). With that password the passwords TablePlus exported come as [`Secret`]s,
//!   on their way to the keychain only; a wrong password is refused before anything is read.
//!   The export also carries an SSH private key's contents, which SpelaionDB never stores.
//!
//! Either way an SSH key is imported as key authentication with SpelaionDB's usual keys
//! (`~/.ssh`) until the user picks the file, and the preview says so.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use aes::Aes256;
use cbc::Decryptor;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, KeyInit, Mac};
use plist::{Dictionary, Value};
use serde_json::Value as Json;
use sha1::Sha1;
use sha2::Sha256;
use spelaion_driver_api::Secret;

use crate::import::{ImportedConnection, ImportedSsh, ImportedTls};
use crate::model::{Engine, SshAuth, TlsModeConfig};

/// TablePlus's application id: the name of its folder on every system.
const APP_DIR: &str = "com.tinyapp.TablePlus";

/// The largest file read: a TablePlus export or storage file is a few kilobytes per
/// connection; anything bigger is not one.
const MAX_FILE: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TableplusError {
    #[error(
        "no TablePlus connections under {0} (Data/Connections.plist in TablePlus's folder, or an exported .tableplusconnection file)"
    )]
    NotFound(PathBuf),
    #[error("could not read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
    /// An export file, encrypted with a password the user has not given yet.
    #[error("{0} is encrypted: type the password it was exported with")]
    NeedsPassword(PathBuf),
    /// The password given does not open the export file.
    #[error("the password does not open {0}")]
    WrongPassword(PathBuf),
}

fn unreadable(path: &Path, e: impl std::fmt::Display) -> TableplusError {
    TableplusError::Unreadable {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// Where TablePlus keeps its connections on this system (it may not exist): `%LOCALAPPDATA%`
/// on Windows, `~/Library/Application Support` on macOS.
pub fn tableplus_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(APP_DIR))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Application Support")
                .join(APP_DIR)
        })
    } else {
        // TablePlus for Linux is a preview; it is not known to keep connections in a file.
        None
    }
}

/// The connections TablePlus keeps under `path`: its folder, its `Data` folder, a
/// `Connections.plist`, or an exported `.tableplusconnection` file (opened with `password`,
/// the one it was exported with).
pub fn read_tableplus(
    path: &Path,
    password: Option<&Secret>,
) -> Result<Vec<ImportedConnection>, TableplusError> {
    let file = connections_file(path).ok_or_else(|| TableplusError::NotFound(path.into()))?;
    let data = read_bounded(&file)?;
    match kind_of(&data) {
        Kind::Plist => read_storage(&file, &data),
        Kind::Json => read_export(&file, &data),
        Kind::Encrypted => {
            let password = password
                .filter(|p| !p.expose().is_empty())
                .ok_or_else(|| TableplusError::NeedsPassword(file.clone()))?;
            let plain =
                decrypt_export(&data, password.expose().as_bytes()).map_err(|e| match e {
                    Decrypt::WrongPassword => TableplusError::WrongPassword(file.clone()),
                    Decrypt::Malformed(why) => unreadable(&file, why),
                })?;
            read_export(&file, &plain)
        }
        Kind::Unknown => Err(unreadable(&file, "not a TablePlus connections file")),
    }
}

/// The file under `path`, whichever of TablePlus's folders or files it names.
fn connections_file(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    [
        path.join("Connections.plist"),
        path.join("Data").join("Connections.plist"),
    ]
    .into_iter()
    .find(|file| file.is_file())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, TableplusError> {
    let file = std::fs::File::open(path).map_err(|e| unreadable(path, e))?;
    let mut data = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut data)
        .map_err(|e| unreadable(path, e))?;
    if data.len() as u64 > MAX_FILE {
        return Err(unreadable(path, "too large for a TablePlus file"));
    }
    Ok(data)
}

enum Kind {
    Plist,
    Json,
    Encrypted,
    Unknown,
}

fn kind_of(data: &[u8]) -> Kind {
    let text = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    let first = text.iter().find(|b| !b.is_ascii_whitespace());
    if data.starts_with(b"bplist") || first == Some(&b'<') {
        Kind::Plist
    } else if first == Some(&b'[') {
        Kind::Json
    } else if data.len() >= 2 && data[0] == 3 && data[1] == 1 {
        Kind::Encrypted
    } else {
        Kind::Unknown
    }
}

enum Decrypt {
    WrongPassword,
    Malformed(&'static str),
}

/// Opens RNCryptor's v3 password format: version 3, options 1, an 8-byte encryption salt, an
/// 8-byte HMAC salt, a 16-byte IV, the AES-256-CBC ciphertext and its HMAC-SHA256 over all
/// that comes before it. Both keys are PBKDF2-SHA1 of the password, 10,000 rounds. The HMAC is
/// checked first, so a wrong password (or a changed file) decrypts nothing.
fn decrypt_export(data: &[u8], password: &[u8]) -> Result<Vec<u8>, Decrypt> {
    const HEADER: usize = 2 + 8 + 8 + 16;
    const TAG: usize = 32;
    if data.len() < HEADER + 16 + TAG || !(data.len() - HEADER - TAG).is_multiple_of(16) {
        return Err(Decrypt::Malformed("not a complete encrypted export"));
    }
    let (signed, tag) = data.split_at(data.len() - TAG);
    let (encryption_salt, hmac_salt, iv) = (&data[2..10], &data[10..18], &data[18..34]);
    let key = |salt: &[u8]| {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha1>(password, salt, 10_000, &mut key);
        key
    };
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key(hmac_salt))
        .map_err(|_| Decrypt::Malformed("HMAC key"))?;
    mac.update(signed);
    mac.verify_slice(tag).map_err(|_| Decrypt::WrongPassword)?;
    let mut plain = data[HEADER..signed.len()].to_vec();
    let len = Decryptor::<Aes256>::new_from_slices(&key(encryption_salt), iv)
        .map_err(|_| Decrypt::Malformed("AES key"))?
        .decrypt_padded_mut::<Pkcs7>(&mut plain)
        .map_err(|_| Decrypt::Malformed("bad padding"))?
        .len();
    plain.truncate(len);
    Ok(plain)
}

/// Which of TablePlus's files a connection came from: what is in it differs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Storage,
    Export,
}

fn read_storage(file: &Path, data: &[u8]) -> Result<Vec<ImportedConnection>, TableplusError> {
    let connections = plist_array(file, data)?;
    let groups_file = file
        .parent()
        .map(|dir| dir.join("ConnectionGroups.plist"))
        .filter(|groups| groups.is_file());
    let groups = match groups_file {
        Some(path) => plist_array(&path, &read_bounded(&path)?)?,
        None => Vec::new(),
    };
    let folders = folders(&groups);
    Ok(connections
        .iter()
        .filter_map(Value::as_dictionary)
        .filter(|c| !text(c, "ID").is_empty())
        .map(|c| {
            let folder = folders
                .get(&text(c, "GroupID"))
                .cloned()
                .unwrap_or_default();
            connection_of(c, text(c, "ID"), folder, Origin::Storage)
        })
        .collect())
}

/// A property list holding an array (TablePlus writes XML; a binary one, as macOS may write,
/// reads the same).
fn plist_array(path: &Path, data: &[u8]) -> Result<Vec<Value>, TableplusError> {
    match Value::from_reader(std::io::Cursor::new(data)).map_err(|e| unreadable(path, e))? {
        Value::Array(items) => Ok(items),
        _ => Err(unreadable(path, "not a list of connections")),
    }
}

/// An export: a JSON array of connections and of groups, each group with its `connections`
/// and nested `groups`. Connections carry no id there: they are numbered in the file's order,
/// which is the same every time the file is read.
fn read_export(file: &Path, data: &[u8]) -> Result<Vec<ImportedConnection>, TableplusError> {
    let items: Vec<Json> = serde_json::from_slice(data).map_err(|e| unreadable(file, e))?;
    let mut out = Vec::new();
    walk_export(&items, &[], &mut out);
    Ok(out)
}

fn walk_export(items: &[Json], folder: &[String], out: &mut Vec<ImportedConnection>) {
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        if object.contains_key("connections") || object.contains_key("groups") {
            let name = object
                .get("Name")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .trim()
                .replace('/', " ");
            let mut inner = folder.to_vec();
            if !name.is_empty() {
                inner.push(name);
            }
            for key in ["groups", "connections"] {
                if let Some(Json::Array(children)) = object.get(key) {
                    walk_export(children, &inner, out);
                }
            }
        } else if let Value::Dictionary(c) = plist_of(item) {
            let id = format!("export-{}", out.len() + 1);
            out.push(connection_of(&c, id, folder.join("/"), Origin::Export));
        }
    }
}

/// JSON as a property list value, so both of TablePlus's files are read by the same code.
fn plist_of(json: &Json) -> Value {
    match json {
        Json::Null => Value::String(String::new()),
        Json::Bool(b) => Value::Boolean(*b),
        Json::Number(n) => n
            .as_i64()
            .map(|i| Value::Integer(i.into()))
            .unwrap_or_else(|| Value::Real(n.as_f64().unwrap_or_default())),
        Json::String(s) => Value::String(s.clone()),
        Json::Array(items) => Value::Array(items.iter().map(plist_of).collect()),
        Json::Object(map) => {
            Value::Dictionary(map.iter().map(|(k, v)| (k.clone(), plist_of(v))).collect())
        }
    }
}

/// A setting as text: strings trimmed, numbers written out, anything else empty.
fn text(dict: &Dictionary, key: &str) -> String {
    match dict.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Integer(n)) => n.to_string(),
        Some(Value::Real(n)) => n.to_string(),
        Some(Value::Boolean(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// A setting as a number: TablePlus writes flags and modes as integers (`0`/`1`), a few as
/// strings or booleans.
fn number(dict: &Dictionary, key: &str) -> i64 {
    match dict.get(key) {
        Some(Value::Integer(n)) => n.as_signed().unwrap_or_default(),
        Some(Value::Boolean(b)) => i64::from(*b),
        Some(Value::String(s)) => s.trim().parse().unwrap_or_default(),
        _ => 0,
    }
}

/// A password as TablePlus exported it: never trimmed, `None` when empty.
fn secret(dict: &Dictionary, key: &str) -> Option<Secret> {
    match dict.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(Secret::new(s.clone())),
        _ => None,
    }
}

fn port_of(dict: &Dictionary, key: &str) -> Option<u16> {
    text(dict, key).parse().ok().filter(|p| *p > 0)
}

/// The folder path (`a/b`) of every group by id; a group inside an unknown one, or in a loop,
/// stops at what is known.
fn folders(groups: &[Value]) -> HashMap<String, String> {
    let by_id: HashMap<String, (String, String)> = groups
        .iter()
        .filter_map(Value::as_dictionary)
        .map(|g| (text(g, "ID"), (text(g, "Name"), text(g, "GroupID"))))
        .filter(|(id, _)| !id.is_empty())
        .collect();
    by_id
        .keys()
        .map(|id| {
            let mut parts = Vec::new();
            let mut seen = Vec::new();
            let mut next = id.clone();
            while let Some((name, parent)) = by_id.get(&next) {
                if seen.contains(&next) {
                    break;
                }
                seen.push(next.clone());
                parts.push(name.replace('/', " "));
                next = parent.clone();
            }
            parts.reverse();
            (id.clone(), parts.join("/"))
        })
        .collect()
}

fn engine_of(driver: &str) -> Result<Engine, String> {
    let key: String = driver
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    match key.as_str() {
        "postgresql" | "postgres" => Ok(Engine::Postgres),
        "mysql" => Ok(Engine::Mysql),
        "mariadb" => Ok(Engine::Mariadb),
        "microsoftsqlserver" | "sqlserver" | "mssql" => Ok(Engine::Sqlserver),
        "sqlite" => Ok(Engine::Sqlite),
        "duckdb" => Ok(Engine::Duckdb),
        "" => Err("a connection without a driver".into()),
        _ => Err(format!("{driver} is not supported yet")),
    }
}

/// TablePlus's TLS mode (`tLSMode`): the position in the driver's list in its form.
/// PostgreSQL and MySQL list PREFERRED, DISABLED, REQUIRED, ALLOW, VERIFY-CA and
/// VERIFY-FULL (VERIFY-IDENTITY on MySQL); MariaDB lists PREFERRED, ENFORCE and
/// VERIFY-SERVER-CERT. `None` keeps SpelaionDB's default (prefer TLS), as TablePlus's
/// PREFERRED does.
fn tls_mode(engine: Engine, mode: i64, unmapped: &mut Vec<String>) -> Option<TlsModeConfig> {
    match engine {
        Engine::Postgres | Engine::Mysql => match mode {
            0 => None,
            1 => Some(TlsModeConfig::Disable),
            2 => Some(TlsModeConfig::Require),
            3 => {
                unmapped.push("TLS: mode ALLOW (imported as Prefer)".into());
                None
            }
            4 => Some(TlsModeConfig::VerifyCa),
            5 => Some(TlsModeConfig::VerifyFull),
            other => {
                unmapped.push(format!("TLS: mode {other} (SpelaionDB's default is kept)"));
                None
            }
        },
        Engine::Mariadb => match mode {
            0 => None,
            1 => Some(TlsModeConfig::Require),
            2 => Some(TlsModeConfig::VerifyFull),
            other => {
                unmapped.push(format!("TLS: mode {other} (SpelaionDB's default is kept)"));
                None
            }
        },
        // TablePlus has no TLS settings for these.
        Engine::Sqlserver | Engine::Sqlite | Engine::Duckdb => None,
    }
}

const PASSWORD_NOT_READ: &str = "Password: if TablePlus saved one, it is in its own encrypted store, which SpelaionDB does not read; it is asked when connecting";

fn connection_of(c: &Dictionary, id: String, folder: String, origin: Origin) -> ImportedConnection {
    let driver = text(c, "Driver");
    let engine = engine_of(&driver);
    let file = engine.as_ref().is_ok_and(|e| e.is_file());
    let mut unmapped = Vec::new();

    // SQLite and DuckDB: the file is in `DatabaseHost` (`DatabaseName` is only its name).
    let (host, port, database, user) = if file {
        (String::new(), None, text(c, "DatabaseHost"), String::new())
    } else {
        (
            text(c, "DatabaseHost"),
            port_of(c, "DatabasePort"),
            text(c, "DatabaseName"),
            text(c, "DatabaseUser"),
        )
    };

    let tls = match &engine {
        Ok(engine) if !file => {
            let mode = tls_mode(*engine, number(c, "tLSMode"), &mut unmapped);
            // The files, in the order TablePlus keeps them: key, certificate, CA.
            let paths: Vec<String> = match c.get("TlsKeyPaths") {
                Some(Value::Array(paths)) => paths
                    .iter()
                    .map(|p| p.as_string().unwrap_or_default().trim().to_string())
                    .collect(),
                _ => Vec::new(),
            };
            let path = |i: usize| paths.get(i).cloned().unwrap_or_default();
            let (key, cert, ca) = (path(0), path(1), path(2));
            let has_files = !(key.is_empty() && cert.is_empty() && ca.is_empty());
            match (mode, has_files) {
                (None, false) => None,
                (mode, _) => Some(ImportedTls {
                    mode: mode.unwrap_or_default(),
                    ca,
                    cert,
                    key,
                }),
            }
        }
        _ => None,
    };
    if !file && number(c, "tLSNegotiation") != 0 {
        unmapped.push("TLS: direct TLS negotiation".into());
    }
    if !text(c, "TlsKeyPassword").is_empty() {
        unmapped.push("TLS: the client key's password (not imported)".into());
    }
    let sni = text(c, "ServerNameIndication");
    if !sni.is_empty() {
        unmapped.push(format!("TLS: server name {sni}"));
    }

    let ssh = (number(c, "isOverSSH") != 0).then(|| {
        let key = number(c, "isUsePrivateKey") != 0;
        if key {
            let key_name = text(c, "ServerPrivateKeyName");
            let what = if !text(c, "PrivateKeyData").is_empty() {
                "SSH: the export carries the private key itself, which SpelaionDB does not store".to_string()
            } else if !key_name.is_empty() {
                format!("SSH: TablePlus keeps the private key {key_name} in its own store, not its path")
            } else {
                "SSH: TablePlus does not say where the private key is".to_string()
            };
            unmapped.push(format!(
                "{what}; SpelaionDB tries your keys in ~/.ssh until you pick the file in the network profile"
            ));
            if !text(c, "PrivateKeyPass").is_empty() {
                unmapped.push("SSH: the private key's passphrase (asked when connecting)".into());
            }
        }
        ImportedSsh {
            host: text(c, "ServerAddress"),
            port: port_of(c, "ServerPort"),
            user: text(c, "ServerUser"),
            auth: if key { SshAuth::Key } else { SshAuth::Password },
            key_path: String::new(),
            password: if key { None } else { secret(c, "ServerPassword") },
            keepalive_interval: None,
            keepalive_count: None,
            connect_timeout: None,
            local_port: None,
        }
    });

    let password = if file {
        None
    } else {
        secret(c, "DatabasePassword")
    };
    if engine.is_ok() && !file && origin == Origin::Storage {
        unmapped.push(PASSWORD_NOT_READ.into());
    }
    let commands = text(c, "StartupCommands");
    if !commands.is_empty() {
        let lines = commands.lines().filter(|l| !l.trim().is_empty()).count();
        unmapped.push(format!(
            "Bootstrap commands: {lines} line(s) TablePlus runs after connecting (add them to the connection's session setup)"
        ));
    }
    if !text(c, "StartupBashScript").is_empty() {
        unmapped.push("Bootstrap script: a shell script TablePlus runs (never run here)".into());
    }
    if number(c, "SafeMode") != 0 || number(c, "AdvancedSafeMode") != 0 {
        unmapped.push(
            "Safe mode: TablePlus asks before running queries (a connection type such as Production does that here)"
                .into(),
        );
    }
    if number(c, "UseSock") != 0 {
        unmapped.push("Socket: TablePlus connects through a local socket".into());
    }
    let role = text(c, "DatabaseUserRole");
    if !role.is_empty() && !role.eq_ignore_ascii_case("default") {
        unmapped.push(format!("Role: {role}"));
    }
    for (key, what) in [
        ("Authenticator", "Authentication"),
        ("DatabaseWarehouse", "Warehouse"),
        ("DatabaseEncoding", "Encoding"),
    ] {
        let value = text(c, key);
        if !value.is_empty() {
            unmapped.push(format!("{what}: {value}"));
        }
    }
    if let Some(Value::Array(options)) = c.get("OtherOptions") {
        for option in options {
            if let Some(option) = option.as_string().filter(|o| !o.trim().is_empty()) {
                unmapped.push(format!("Option: {}", option.trim()));
            }
        }
    }

    ImportedConnection {
        id,
        name: text(c, "ConnectionName"),
        description: String::new(),
        tags: Vec::new(),
        folder,
        engine,
        host,
        port,
        database,
        user,
        password,
        stored_password: None,
        // TablePlus's misspelled key, on every system.
        env: crate::dbeaver::closest_type(&text(c, "Enviroment"), None),
        read_only: text(c, "DatabaseType").eq_ignore_ascii_case("read_only"),
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
    fn drivers_map_to_engines() {
        assert_eq!(engine_of("PostgreSQL"), Ok(Engine::Postgres));
        assert_eq!(engine_of("MySQL"), Ok(Engine::Mysql));
        assert_eq!(engine_of("MariaDB"), Ok(Engine::Mariadb));
        assert_eq!(engine_of("MicrosoftSQLServer"), Ok(Engine::Sqlserver));
        assert_eq!(engine_of("SQLite"), Ok(Engine::Sqlite));
        assert_eq!(engine_of("DuckDB"), Ok(Engine::Duckdb));
        assert_eq!(
            engine_of("Redis"),
            Err("Redis is not supported yet".to_string())
        );
    }

    #[test]
    fn tls_modes_follow_each_drivers_list() {
        let mut unmapped = Vec::new();
        assert_eq!(tls_mode(Engine::Postgres, 0, &mut unmapped), None);
        assert_eq!(
            tls_mode(Engine::Postgres, 4, &mut unmapped),
            Some(TlsModeConfig::VerifyCa)
        );
        assert_eq!(
            tls_mode(Engine::Mysql, 2, &mut unmapped),
            Some(TlsModeConfig::Require)
        );
        assert_eq!(
            tls_mode(Engine::Mariadb, 2, &mut unmapped),
            Some(TlsModeConfig::VerifyFull)
        );
        assert!(unmapped.is_empty());
        assert_eq!(tls_mode(Engine::Postgres, 3, &mut unmapped), None);
        assert_eq!(tls_mode(Engine::Mysql, 9, &mut unmapped), None);
        assert_eq!(unmapped.len(), 2, "{unmapped:?}");
    }

    #[test]
    fn nested_groups_become_folders_and_loops_stop() {
        let group = |id: &str, name: &str, parent: &str| {
            let mut d = Dictionary::new();
            d.insert("ID".into(), Value::String(id.into()));
            d.insert("Name".into(), Value::String(name.into()));
            d.insert("GroupID".into(), Value::String(parent.into()));
            Value::Dictionary(d)
        };
        let folders = folders(&[
            group("a", "Prod", ""),
            group("b", "EU/West", "a"),
            group("x", "Loop", "y"),
            group("y", "Back", "x"),
        ]);
        assert_eq!(folders["a"], "Prod");
        assert_eq!(folders["b"], "Prod/EU West");
        assert!(folders["x"].split('/').count() <= 2, "{}", folders["x"]);
    }

    #[test]
    fn files_are_told_apart_by_their_first_bytes() {
        assert!(matches!(kind_of(b"<?xml version"), Kind::Plist));
        assert!(matches!(kind_of(b"bplist00"), Kind::Plist));
        assert!(matches!(kind_of(b"\xEF\xBB\xBF  [{}]"), Kind::Json));
        assert!(matches!(kind_of(&[3, 1, 0, 0]), Kind::Encrypted));
        assert!(matches!(kind_of(b"hello"), Kind::Unknown));
        assert!(matches!(kind_of(b""), Kind::Unknown));
    }

    #[test]
    fn truncated_or_misaligned_exports_are_refused_without_decrypting() {
        assert!(matches!(
            decrypt_export(&[3, 1, 0, 0], b"pw"),
            Err(Decrypt::Malformed(_))
        ));
        let misaligned = vec![3u8; 2 + 8 + 8 + 16 + 17 + 32];
        assert!(matches!(
            decrypt_export(&misaligned, b"pw"),
            Err(Decrypt::Malformed(_))
        ));
        // Well formed but signed with another key: a wrong password.
        let mut forged = vec![0u8; 2 + 8 + 8 + 16 + 16 + 32];
        forged[0] = 3;
        forged[1] = 1;
        assert!(matches!(
            decrypt_export(&forged, b"pw"),
            Err(Decrypt::WrongPassword)
        ));
    }
}
