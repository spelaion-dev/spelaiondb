//! Connections imported from Beekeeper Studio: its own SQLite database, `app.db`, in its data
//! folder (`%APPDATA%\beekeeper-studio` on Windows, `~/Library/Application Support/beekeeper-studio`
//! on macOS, `~/.config/beekeeper-studio` on Linux), opened read-only. Saved connections are the
//! `saved_connection` table; folders, `connection_folder` (nested through `parentId`).
//!
//! Passwords: Beekeeper encrypts them (with its `simple-encryptor` library: AES-256-CBC under
//! SHA-256 of a key, with an HMAC-SHA256 checked first) using a random key it generates per
//! install and keeps in the data folder's `.key` file, itself encrypted with a key built into
//! Beekeeper (its open source `encryption_key.ts`). This reader takes the same steps Beekeeper
//! takes to read them, with the files it reads, so it can import what Beekeeper itself would
//! use: like DBeaver's (`dbeaver.rs`), this obscures, it does not protect. A decrypted password
//! only lives as a [`Secret`] on its way to the keychain. Only the database password and the SSH
//! password are decrypted; other saved secrets (a key passphrase, the jump host's, the URL)
//! are never decrypted, only listed.
//!
//! A connection's label colour is listed, never imported: SpelaionDB has no free colours
//! (ADR-029).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aes::Aes256;
use cbc::Decryptor;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use spelaion_driver_api::Secret;

use crate::datagrip::shown;
use crate::import::{ImportedConnection, ImportedSsh, ImportedTls};
use crate::model::{Engine, SshAuth, TlsModeConfig};

/// Beekeeper's own database, in its data folder.
const DATABASE_FILE: &str = "app.db";

/// The file that holds this install's encryption key, next to `app.db`.
const KEY_FILE: &str = ".key";

/// The key built into Beekeeper Studio that encrypts `.key` (`encryption_key.ts`).
const BUILT_IN_KEY: &str = "38782F413F442A472D4B6150645367566B59703373367639792442264529482B";

/// The most connections read.
const MAX_CONNECTIONS: usize = 10_000;

/// The largest `.key` file read: Beekeeper's is a few hundred bytes.
const MAX_KEY_FILE: u64 = 64 * 1024;

/// The deepest folder nesting followed (folders cannot contain themselves in Beekeeper, but a
/// crafted file could say so).
const MAX_FOLDER_DEPTH: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum BeekeeperError {
    #[error("no Beekeeper Studio connections under {0} (app.db in Beekeeper's data folder)")]
    NotFound(PathBuf),
    #[error("could not read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
}

fn unreadable(path: &Path, e: impl std::fmt::Display) -> BeekeeperError {
    BeekeeperError::Unreadable {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// Beekeeper Studio's data folder on this system, where `app.db` is (it may not exist). The
/// portable build keeps its data in `beekeeper_studio_data` next to the executable instead:
/// that folder can be picked by hand.
pub fn beekeeper_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
        })
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    };
    base.map(|base| base.join("beekeeper-studio"))
}

/// The saved connections under `path`: Beekeeper's data folder (its `app.db` and `.key`), or
/// the `app.db` file itself (its `.key` is looked for next to it).
pub fn read_beekeeper(path: &Path) -> Result<Vec<ImportedConnection>, BeekeeperError> {
    let file = if path.is_dir() {
        path.join(DATABASE_FILE)
    } else {
        path.to_path_buf()
    };
    if !file.is_file() {
        return Err(BeekeeperError::NotFound(path.to_path_buf()));
    }
    let key = file
        .parent()
        .map(|dir| read_key(&dir.join(KEY_FILE)))
        .unwrap_or(KeyState::Missing);
    read_database(&file, &key)
}

/// This install's key, as far as it could be read.
enum KeyState {
    Missing,
    Unreadable,
    Found(Secret),
}

fn read_key(path: &Path) -> KeyState {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return KeyState::Missing;
    };
    let mut text = String::new();
    if file
        .take(MAX_KEY_FILE + 1)
        .read_to_string(&mut text)
        .is_err()
        || text.len() as u64 > MAX_KEY_FILE
    {
        return KeyState::Unreadable;
    }
    match decrypt(BUILT_IN_KEY, text.trim()) {
        Some(Json::Object(map)) => match map.get("encryptionKey") {
            Some(Json::String(key)) if !key.is_empty() => KeyState::Found(Secret::new(key)),
            _ => KeyState::Unreadable,
        },
        _ => KeyState::Unreadable,
    }
}

/// `simple-encryptor`'s `decrypt`: `<hmac hex, 64><iv hex, 32><AES-256-CBC, base64>`, the key
/// being SHA-256 of the key text, the HMAC checked before anything is decrypted. The plain
/// text is JSON. `None` for anything that does not decrypt.
fn decrypt(key: &str, text: &str) -> Option<Json> {
    let key = Sha256::digest(key.as_bytes());
    let (tag, rest) = (text.get(..64)?, text.get(64..)?);
    let tag = hex(tag)?;
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key[..]).ok()?;
    mac.update(rest.as_bytes());
    mac.verify_slice(&tag).ok()?;
    let iv = hex(rest.get(..32)?)?;
    let mut data = base64(rest.get(32..)?)?;
    let plain = Decryptor::<Aes256>::new_from_slices(&key[..], &iv)
        .ok()?
        .decrypt_padded_mut::<Pkcs7>(&mut data)
        .ok()?;
    serde_json::from_slice(plain).ok()
}

fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Standard base64 with padding, as Node writes it.
fn base64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = i + 1 == bytes.len() / 4;
        let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (j, c) in chunk.iter().enumerate() {
            let v = if j >= 4 - pad { 0 } else { value(*c)? };
            n = (n << 6) | v;
        }
        let decoded = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&decoded[..3 - pad]);
    }
    Some(out)
}

/// One cell of a row, read leniently.
#[derive(Debug, Clone)]
enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
}

/// Columns holding a secret: never shown; only the database and SSH passwords are decrypted.
const SECRET_COLUMNS: &[&str] = &[
    "password",
    "sshPassword",
    "sshKeyfilePassword",
    "sshBastionPassword",
    "sshBastionKeyfilePassword",
    "url",
];

fn read_database(path: &Path, key: &KeyState) -> Result<Vec<ImportedConnection>, BeekeeperError> {
    use rusqlite::{OpenFlags, types::ValueRef};
    let db = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| unreadable(path, e))?;
    let _ = db.busy_timeout(std::time::Duration::from_secs(2));
    let not_beekeeper = |_| unreadable(path, "not Beekeeper Studio's database");
    let columns_of = |table: &str| -> Result<Vec<String>, BeekeeperError> {
        db.prepare("SELECT name FROM pragma_table_info(?1)")
            .and_then(|mut s| {
                s.query_map([table], |r| r.get::<_, String>(0))?
                    .collect::<Result<_, _>>()
            })
            .map_err(not_beekeeper)
    };
    let columns = columns_of("saved_connection")?;
    if !["name", "connectionType"]
        .iter()
        .all(|c| columns.iter().any(|x| x == c))
    {
        return Err(unreadable(
            path,
            "not Beekeeper Studio's database (no saved connections)",
        ));
    }
    let folders = read_folders(&db, &columns_of("connection_folder")?, path)?;
    // Only names from the table's own columns are written into the statement, quoted.
    let fields: Vec<String> = columns
        .iter()
        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
        .collect();
    // In the order they were saved (`position` only orders a folder's own list).
    let sql = format!(
        "SELECT {}, rowid FROM saved_connection ORDER BY rowid LIMIT {MAX_CONNECTIONS}",
        fields.join(", ")
    );
    let mut statement = db.prepare(&sql).map_err(not_beekeeper)?;
    let count = columns.len();
    let rows = statement
        .query_map([], |row| {
            let mut cells = HashMap::new();
            for (i, column) in columns.iter().enumerate() {
                let cell = match row.get_ref(i)? {
                    ValueRef::Null => Cell::Null,
                    ValueRef::Integer(n) => Cell::Integer(n),
                    ValueRef::Real(f) => Cell::Real(f),
                    ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(_) => Cell::Text("(binary)".into()),
                };
                cells.insert(column.clone(), cell);
            }
            let rowid: i64 = row.get(count)?;
            Ok((rowid, cells))
        })
        .map_err(|e| unreadable(path, e))?;
    let mut connections = Vec::new();
    for row in rows {
        let (rowid, cells) = row.map_err(|e| unreadable(path, e))?;
        connections.push(connection_of(rowid, &Row(cells), &folders, key));
    }
    Ok(connections)
}

/// Every folder's path (`Production/EU`), by id.
fn read_folders(
    db: &rusqlite::Connection,
    columns: &[String],
    path: &Path,
) -> Result<HashMap<i64, String>, BeekeeperError> {
    // Older versions have no folders.
    if !["id", "name"]
        .iter()
        .all(|c| columns.iter().any(|x| x == c))
    {
        return Ok(HashMap::new());
    }
    let parent = if columns.iter().any(|c| c == "parentId") {
        "\"parentId\""
    } else {
        "NULL"
    };
    let mut statement = db
        .prepare(&format!(
            "SELECT id, name, {parent} FROM connection_folder LIMIT {MAX_CONNECTIONS}"
        ))
        .map_err(|e| unreadable(path, e))?;
    let rows: Vec<(i64, String, Option<i64>)> = statement
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get(2)?,
            ))
        })
        .and_then(|rows| rows.collect())
        .map_err(|e| unreadable(path, e))?;
    let by_id: HashMap<i64, (String, Option<i64>)> = rows
        .into_iter()
        .map(|(id, name, parent)| (id, (name, parent)))
        .collect();
    Ok(by_id
        .keys()
        .map(|id| {
            let mut parts = Vec::new();
            let mut next = Some(*id);
            while let Some(current) = next {
                if parts.len() >= MAX_FOLDER_DEPTH {
                    break;
                }
                let Some((name, parent)) = by_id.get(&current) else {
                    break;
                };
                // A folder name is one level: a slash in it is not a nesting.
                parts.push(name.trim().replace('/', "-"));
                next = *parent;
            }
            parts.reverse();
            (*id, parts.join("/"))
        })
        .collect())
}

struct Row(HashMap<String, Cell>);

impl Row {
    fn cell(&self, column: &str) -> &Cell {
        self.0.get(column).unwrap_or(&Cell::Null)
    }

    fn text(&self, column: &str) -> String {
        match self.cell(column) {
            Cell::Text(s) => s.trim().to_string(),
            Cell::Integer(n) => n.to_string(),
            Cell::Real(f) => f.to_string(),
            Cell::Null => String::new(),
        }
    }

    fn number(&self, column: &str) -> Option<i64> {
        match self.cell(column) {
            Cell::Integer(n) => Some(*n),
            Cell::Real(f) if f.fract() == 0.0 => Some(*f as i64),
            Cell::Text(s) => s.trim().parse().ok(),
            Cell::Real(_) | Cell::Null => None,
        }
    }

    fn flag(&self, column: &str) -> bool {
        match self.cell(column) {
            Cell::Integer(n) => *n != 0,
            Cell::Real(f) => *f != 0.0,
            Cell::Text(s) => matches!(s.trim(), "1" | "true"),
            Cell::Null => false,
        }
    }

    /// A JSON object column (`options`, `azureAuthOptions`…); empty for anything else.
    fn object(&self, column: &str) -> serde_json::Map<String, Json> {
        match serde_json::from_str(&self.text(column)) {
            Ok(Json::Object(map)) => map,
            _ => serde_json::Map::new(),
        }
    }
}

fn port_of(value: Option<i64>) -> Option<u16> {
    value.and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0)
}

/// A value for the preview: without control characters, at most 80 of them.
fn shown_value(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).take(80).collect()
}

/// Beekeeper's connection types, by its id: ours, or why not.
fn engine_of(kind: &str) -> Result<Engine, String> {
    // Older versions wrote the aliases Beekeeper still maps (`parseConnectionType`).
    match kind.trim().to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" | "psql" => Ok(Engine::Postgres),
        "mysql" => Ok(Engine::Mysql),
        "mariadb" => Ok(Engine::Mariadb),
        "sqlserver" | "mssql" => Ok(Engine::Sqlserver),
        "sqlite" => Ok(Engine::Sqlite),
        "duckdb" => Ok(Engine::Duckdb),
        other => {
            let name = match other {
                "tidb" => "TiDB".to_string(),
                "starrocks" => "StarRocks".into(),
                "libsql" => "LibSQL".into(),
                "redshift" => "Amazon Redshift".into(),
                "cockroachdb" => "CockroachDB".into(),
                "greengage" => "GreengageDB".into(),
                "oracle" => "Oracle".into(),
                "cassandra" => "Cassandra".into(),
                "scylladb" => "ScyllaDB".into(),
                "bigquery" => "BigQuery".into(),
                "firebird" => "Firebird".into(),
                "clickhouse" => "ClickHouse".into(),
                "mongodb" => "MongoDB".into(),
                "sqlanywhere" => "SQL Anywhere".into(),
                "trino" => "Trino".into(),
                "surrealdb" => "SurrealDB".into(),
                "redis" => "Redis".into(),
                "bedrock" => "Bedrock".into(),
                "dynamodb" => "DynamoDB".into(),
                "snowflake" => "Snowflake".into(),
                "" => "a connection without a type".into(),
                unknown => shown_value(unknown),
            };
            Err(format!("{name} is not supported yet"))
        }
    }
}

/// The columns this reader maps, or that are Beekeeper's bookkeeping or defaults: any other
/// column holding something is listed as not imported.
const KNOWN: &[&str] = &[
    "id",
    "createdAt",
    "updatedAt",
    "version",
    "uniqueHash",
    "position",
    "workspaceId",
    "name",
    "connectionType",
    "host",
    "port",
    "username",
    "defaultDatabase",
    "connectionFolderId",
    "readOnlyMode",
    "labelColor",
    "ssl",
    "sslCaFile",
    "sslCertFile",
    "sslKeyFile",
    "sslRejectUnauthorized",
    "trustServerCertificate",
    "socketPathEnabled",
    "socketPath",
    "domain",
    "windowsAuthEnabled",
    "serviceName",
    "authId",
    "sshEnabled",
    "sshHost",
    "sshPort",
    "sshMode",
    "sshKeyfile",
    "sshUsername",
    "sshKeepaliveInterval",
    "sshBastionHost",
    "sshBastionHostPort",
    "sshBastionMode",
    "sshBastionUsername",
    "sshBastionKeyfile",
    "rememberPassword",
    "rememberSshPassword",
    "rememberSshKeyfilePassword",
    "password",
    "sshPassword",
    "sshKeyfilePassword",
    "sshBastionPassword",
    "sshBastionKeyfilePassword",
    "url",
    "options",
    "azureAuthOptions",
    "iamAuthOptions",
    "sqlServerOptions",
    // Options of engines SpelaionDB does not open.
    "redshiftOptions",
    "cassandraOptions",
    "bigQueryOptions",
    "libsqlOptions",
    "sqlAnywhereOptions",
    "surrealDbOptions",
    "dynamoDbOptions",
    "snowflakeOptions",
];

/// `options` keys that only say what Beekeeper does by default.
fn default_option(key: &str, value: &Json) -> bool {
    match key {
        "connectionMethod" => value.as_str() == Some("manual"),
        // SQLite: Beekeeper's "Create" button, cleared once connected.
        "createDatabaseFile" => true,
        _ => is_unset(value),
    }
}

fn is_unset(value: &Json) -> bool {
    match value {
        Json::Null => true,
        Json::Bool(b) => !b,
        Json::Number(_) => false,
        Json::String(s) => s.trim().is_empty(),
        Json::Array(a) => a.is_empty(),
        Json::Object(o) => o.is_empty(),
    }
}

fn json_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        Json::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// What a saved, encrypted value became.
enum Saved {
    /// Nothing saved.
    None,
    Read(Secret),
    /// Saved, but not decrypted: why, for the preview.
    NotRead(&'static str),
}

/// Decrypts the secret in `column`, if Beekeeper saved one.
fn saved(row: &Row, column: &str, key: &KeyState) -> Saved {
    let text = row.text(column);
    if text.is_empty() {
        return Saved::None;
    }
    match key {
        KeyState::Missing => Saved::NotRead(
            "encrypted with the key in Beekeeper's .key file, which was not found next to app.db",
        ),
        KeyState::Unreadable => {
            Saved::NotRead("encrypted with the key in Beekeeper's .key file, which did not open")
        }
        KeyState::Found(key) => match decrypt(key.expose(), &text) {
            Some(Json::String(s)) if s.is_empty() => Saved::None,
            Some(Json::String(s)) => Saved::Read(Secret::new(s)),
            Some(Json::Null) => Saved::None,
            _ => Saved::NotRead("it does not decrypt with this install's key"),
        },
    }
}

fn connection_of(
    rowid: i64,
    row: &Row,
    folders: &HashMap<i64, String>,
    key: &KeyState,
) -> ImportedConnection {
    let mut unmapped = Vec::new();
    let kind = row.text("connectionType");
    let mut engine = engine_of(&kind);
    let file = matches!(engine, Ok(Engine::Sqlite | Engine::Duckdb));
    let socket = row.flag("socketPathEnabled") && !file;
    if engine.is_ok() && socket {
        engine = Err(format!(
            "a local socket connection ({}), which SpelaionDB does not open",
            shown_value(&row.text("socketPath"))
        ));
    }
    if engine.is_ok() && row.flag("windowsAuthEnabled") {
        engine =
            Err("Windows authentication, which SpelaionDB does not sign in with (yet)".to_string());
    }
    let importable = engine.is_ok();
    let id = match row.number("id") {
        Some(id) => id.to_string(),
        None => rowid.to_string(),
    };
    let folder = row
        .number("connectionFolderId")
        .and_then(|id| folders.get(&id).cloned())
        .unwrap_or_default();
    if !importable {
        // Nothing to say about a connection that will never be imported.
        return ImportedConnection {
            id,
            name: row.text("name"),
            description: String::new(),
            tags: Vec::new(),
            folder,
            engine,
            host: String::new(),
            port: None,
            database: String::new(),
            user: String::new(),
            password: None,
            stored_password: None,
            env: String::new(),
            read_only: false,
            ssh: None,
            proxy: None,
            tls: None,
            unmapped,
        };
    }

    let workspace = row.number("workspaceId").unwrap_or(-1);
    if workspace != -1 {
        unmapped.push(format!("Beekeeper workspace: {workspace}"));
    }

    let password = match saved(row, "password", key) {
        Saved::None => None,
        Saved::Read(secret) => Some(secret),
        Saved::NotRead(why) => {
            unmapped.push(format!(
                "Password: Beekeeper saved one, {why}; it is asked when connecting"
            ));
            None
        }
    };

    let tls = if file {
        None
    } else {
        tls_of(row, engine.as_ref().ok().copied())
    };

    let ssh = (row.flag("sshEnabled") && !file).then(|| ssh_of(row, key, &mut unmapped));
    if row.flag("sshEnabled") && !row.text("sshBastionHost").is_empty() && !file {
        let user = row.text("sshBastionUsername");
        let at = if user.is_empty() {
            String::new()
        } else {
            format!("{}@", shown_value(&user))
        };
        let port = port_of(row.number("sshBastionHostPort"))
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        unmapped.push(format!(
            "SSH: jump host {at}{}{port} before the SSH host (add it as the tunnel's first hop after importing)",
            shown_value(&row.text("sshBastionHost"))
        ));
    }

    let colour = row.text("labelColor");
    if !colour.is_empty() && colour != "default" {
        unmapped.push(format!("Colour: {}", shown_value(&colour)));
    }
    let domain = row.text("domain");
    if !domain.is_empty() {
        unmapped.push(format!("Domain: {}", shown_value(&domain)));
    }
    if !row.text("url").is_empty() {
        unmapped.push("URL: Beekeeper saved one, encrypted; it is not read".into());
    }
    if row.number("authId").is_some() {
        unmapped.push("Azure sign-in (SpelaionDB signs in with a password)".into());
    }
    for (name, value) in row.object("options") {
        if !default_option(&name, &value) {
            unmapped.push(format!("Option: {}", shown(&name, &json_text(&value))));
        }
    }
    for (column, what) in [
        ("azureAuthOptions", "Azure authentication"),
        ("iamAuthOptions", "IAM authentication"),
        ("sqlServerOptions", "SQL Server option"),
    ] {
        for (name, value) in row.object(column) {
            if !is_unset(&value) {
                unmapped.push(format!("{what}: {}", shown(&name, &json_text(&value))));
            }
        }
    }
    let service = row.text("serviceName");
    if !service.is_empty() {
        unmapped.push(format!("Service name: {}", shown_value(&service)));
    }
    let mut others: Vec<(&String, &Cell)> = row
        .0
        .iter()
        .filter(|(column, cell)| {
            !KNOWN.contains(&column.as_str())
                && match cell {
                    Cell::Null => false,
                    Cell::Integer(n) => *n != 0,
                    Cell::Real(f) => *f != 0.0,
                    Cell::Text(s) => !s.trim().is_empty() && s.trim() != "{}",
                }
        })
        .collect();
    others.sort_by(|a, b| a.0.cmp(b.0));
    for (column, _) in others {
        let value = if SECRET_COLUMNS.contains(&column.as_str()) {
            String::new()
        } else {
            row.text(column)
        };
        unmapped.push(format!("Option: {}", shown(column, &value)));
    }

    ImportedConnection {
        id,
        name: row.text("name"),
        description: String::new(),
        tags: Vec::new(),
        folder,
        engine,
        host: if file {
            String::new()
        } else {
            row.text("host")
        },
        port: if file {
            None
        } else {
            port_of(row.number("port"))
        },
        database: row.text("defaultDatabase"),
        user: if file {
            String::new()
        } else {
            row.text("username")
        },
        password: if file { None } else { password },
        stored_password: None,
        env: String::new(),
        read_only: row.flag("readOnlyMode"),
        ssh,
        proxy: None,
        tls,
        unmapped,
    }
}

/// TLS as Beekeeper applies it. PostgreSQL, MySQL and MariaDB: off, or on and never verified
/// unless certificate files are given and "Reject Unauthorized" is ticked (then the chain and
/// the host name are checked, as Node does). SQL Server: encrypted unless SpelaionDB's
/// default says otherwise, the certificate trusted when "Trust Server Certificate" is ticked
/// (or, with all three files, when "Reject Unauthorized" is not).
fn tls_of(row: &Row, engine: Option<Engine>) -> Option<ImportedTls> {
    let ssl = row.flag("ssl");
    let (ca, cert, key) = (
        row.text("sslCaFile"),
        row.text("sslCertFile"),
        row.text("sslKeyFile"),
    );
    let files = ssl && !(ca.is_empty() && cert.is_empty() && key.is_empty());
    let reject = row.flag("sslRejectUnauthorized");
    let mode = match engine {
        Some(Engine::Sqlserver) => {
            let all_files = ssl && !ca.is_empty() && !cert.is_empty() && !key.is_empty();
            let trust = if all_files {
                !reject
            } else {
                row.flag("trustServerCertificate")
            };
            match (trust, ssl) {
                (false, _) => Some(TlsModeConfig::VerifyFull),
                (true, true) => Some(TlsModeConfig::Require),
                (true, false) => None,
            }
        }
        Some(Engine::Postgres | Engine::Mysql | Engine::Mariadb) => match (ssl, files && reject) {
            (false, _) => None,
            (true, true) => Some(TlsModeConfig::VerifyFull),
            (true, false) => Some(TlsModeConfig::Require),
        },
        Some(Engine::Sqlite | Engine::Duckdb) | None => None,
    };
    let (ca, cert, key) = if ssl {
        (ca, cert, key)
    } else {
        Default::default()
    };
    mode.map(|mode| ImportedTls {
        mode,
        ca,
        cert,
        key,
    })
}

fn ssh_of(row: &Row, key: &KeyState, unmapped: &mut Vec<String>) -> ImportedSsh {
    let mode = row.text("sshMode");
    let auth = match mode.as_str() {
        "keyfile" => SshAuth::Key,
        "userpass" => SshAuth::Password,
        _ => SshAuth::Agent,
    };
    let password = match auth {
        SshAuth::Password => match saved(row, "sshPassword", key) {
            Saved::None => None,
            Saved::Read(secret) => Some(secret),
            Saved::NotRead(why) => {
                unmapped.push(format!(
                    "SSH: Beekeeper saved the SSH password, {why}; it is asked when connecting"
                ));
                None
            }
        },
        SshAuth::Agent | SshAuth::Key | SshAuth::KeyboardInteractive => None,
    };
    if auth == SshAuth::Key {
        // SpelaionDB asks for a key's passphrase when connecting: never decrypted here.
        unmapped.push(if row.text("sshKeyfilePassword").is_empty() {
            "SSH: if the private key has a passphrase, it is asked when connecting".into()
        } else {
            "SSH: Beekeeper saved the key's passphrase, which is not imported; it is asked when connecting".into()
        });
    }
    if auth == SshAuth::Agent && mode != "agent" && !mode.is_empty() {
        unmapped.push(format!(
            "SSH: authentication {} (the agent is used)",
            shown_value(&mode)
        ));
    }
    ImportedSsh {
        host: row.text("sshHost"),
        port: port_of(row.number("sshPort")),
        user: row.text("sshUsername"),
        auth,
        key_path: if auth == SshAuth::Key {
            row.text("sshKeyfile")
        } else {
            String::new()
        },
        password,
        keepalive_interval: row
            .number("sshKeepaliveInterval")
            .and_then(|s| u32::try_from(s).ok())
            .filter(|s| *s > 0),
        keepalive_count: None,
        connect_timeout: None,
        local_port: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_reads_what_node_writes() {
        assert_eq!(base64("").unwrap(), b"");
        assert_eq!(base64("Zg==").unwrap(), b"f");
        assert_eq!(base64("Zm8=").unwrap(), b"fo");
        assert_eq!(base64("Zm9v").unwrap(), b"foo");
        assert_eq!(base64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64("+/8=").unwrap(), [0xfb, 0xff]);
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Zm9*", "Zm9v\n"] {
            assert!(base64(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn hex_reads_pairs_only() {
        assert_eq!(hex("00ff7A").unwrap(), [0, 255, 0x7a]);
        assert!(hex("abc").is_none());
        assert!(hex("zz").is_none());
        // Never splits a multi-byte character.
        assert!(hex("é0").is_none());
    }

    #[test]
    fn anything_that_does_not_decrypt_is_none() {
        assert!(decrypt(BUILT_IN_KEY, "").is_none());
        assert!(decrypt(BUILT_IN_KEY, &"0".repeat(200)).is_none());
        assert!(decrypt(BUILT_IN_KEY, "é".repeat(100).as_str()).is_none());
    }

    #[test]
    fn engines_follow_beekeepers_ids_and_aliases() {
        assert_eq!(engine_of("postgresql"), Ok(Engine::Postgres));
        assert_eq!(engine_of("psql"), Ok(Engine::Postgres));
        assert_eq!(engine_of("mssql"), Ok(Engine::Sqlserver));
        assert_eq!(engine_of("MariaDB"), Ok(Engine::Mariadb));
        assert_eq!(
            engine_of("redshift"),
            Err("Amazon Redshift is not supported yet".into())
        );
        assert_eq!(
            engine_of("future\u{7}db"),
            Err("futuredb is not supported yet".into())
        );
    }
}
