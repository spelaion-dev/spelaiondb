//! Connections imported from pgAdmin 4: the JSON file its Tools → Import/Export Servers →
//! Export writes, or, without one, the desktop app's own `pgadmin4.db` (a SQLite database in
//! `%APPDATA%\pgAdmin` on Windows, `~/.pgadmin` elsewhere), which holds the same servers.
//!
//! Every server is PostgreSQL. A server group is its folder. The export never carries
//! passwords: the preview says each one is asked when connecting. `pgadmin4.db` keeps saved
//! passwords encrypted (with the master password, or a key of pgAdmin's own); this reader opens
//! the database read-only and never selects those columns, only whether one was saved, so the
//! preview can say it is not read.
//!
//! Both sources go through one mapping: a database row is first turned into the object the
//! export would have written for it (`Name`, `Host`, `ConnectionParameters`…), so the same
//! code decides what is imported and what is listed as not imported.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::datagrip::shown;
use crate::import::{ImportedConnection, ImportedSsh, ImportedTls};
use crate::model::{Engine, SshAuth, TlsModeConfig};

/// The largest export read: a few hundred bytes per server.
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// The most servers read from one source.
const MAX_SERVERS: usize = 10_000;

/// pgAdmin's own database, in its data folder.
const DATABASE_FILE: &str = "pgadmin4.db";

/// pgAdmin's default connect timeout, which every new server gets: nothing to say about it.
const DEFAULT_CONNECT_TIMEOUT: i64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum PgadminError {
    #[error(
        "no pgAdmin servers under {0} (pgadmin4.db in pgAdmin's folder, or a file exported with Tools → Import/Export Servers)"
    )]
    NotFound(PathBuf),
    #[error("could not read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
}

fn unreadable(path: &Path, e: impl std::fmt::Display) -> PgadminError {
    PgadminError::Unreadable {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// pgAdmin's data folder on this system, where `pgadmin4.db` is (it may not exist).
pub fn pgadmin_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("pgAdmin"))
    } else {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pgadmin"))
    }
}

/// Where a server came from: it decides what the preview says about its password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// An export: never any password.
    Export,
    /// `pgadmin4.db`: whether a password (and an SSH password) was saved there.
    Database {
        saved_password: bool,
        saved_tunnel_password: bool,
    },
}

/// The servers under `path`: pgAdmin's folder (its `pgadmin4.db`), that database itself, or
/// a JSON file pgAdmin exported.
pub fn read_pgadmin(path: &Path) -> Result<Vec<ImportedConnection>, PgadminError> {
    let file = if path.is_dir() {
        path.join(DATABASE_FILE)
    } else {
        path.to_path_buf()
    };
    if !file.is_file() {
        return Err(PgadminError::NotFound(path.to_path_buf()));
    }
    if is_sqlite(&file)? {
        read_database(&file)
    } else {
        read_export(&file)
    }
}

fn is_sqlite(path: &Path) -> Result<bool, PgadminError> {
    let mut header = [0u8; 16];
    let mut file = std::fs::File::open(path).map_err(|e| unreadable(path, e))?;
    let mut read = 0;
    while read < header.len() {
        match file
            .read(&mut header[read..])
            .map_err(|e| unreadable(path, e))?
        {
            0 => break,
            n => read += n,
        }
    }
    Ok(read == header.len() && &header == b"SQLite format 3\0")
}

fn read_export(path: &Path) -> Result<Vec<ImportedConnection>, PgadminError> {
    let file = std::fs::File::open(path).map_err(|e| unreadable(path, e))?;
    let mut data = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut data)
        .map_err(|e| unreadable(path, e))?;
    if data.len() as u64 > MAX_FILE {
        return Err(unreadable(path, "too large for a pgAdmin export"));
    }
    let root: Value =
        serde_json::from_slice(&data).map_err(|_| unreadable(path, "not a pgAdmin export"))?;
    let servers = root
        .get("Servers")
        .and_then(Value::as_object)
        .ok_or_else(|| unreadable(path, "not a pgAdmin export (no \"Servers\")"))?;
    if servers.len() > MAX_SERVERS {
        return Err(unreadable(path, "too many servers"));
    }
    // pgAdmin numbers them from 1, in its tree's order.
    let mut entries: Vec<(&String, &Value)> = servers.iter().collect();
    entries.sort_by_key(|(id, _)| (id.parse::<u64>().unwrap_or(u64::MAX), (*id).clone()));
    Ok(entries
        .into_iter()
        .filter_map(|(id, server)| {
            server
                .as_object()
                .map(|server| connection_of(id, server, Origin::Export))
        })
        .collect())
}

/// `pgadmin4.db`'s columns and the export's key for each: a row becomes what the export
/// would write for it. Columns not listed and not in [`DATABASE_SKIPPED`] keep their name and
/// are listed as not imported when they hold something.
const DATABASE_KEYS: &[(&str, &str)] = &[
    ("name", "Name"),
    ("host", "Host"),
    ("port", "Port"),
    ("maintenance_db", "MaintenanceDB"),
    ("username", "Username"),
    ("comment", "Comment"),
    ("role", "Role"),
    ("db_res", "DBRestriction"),
    ("db_res_type", "DBRestrictionType"),
    ("bgcolor", "BGColor"),
    ("fgcolor", "FGColor"),
    ("service", "Service"),
    ("use_ssh_tunnel", "UseSSHTunnel"),
    ("tunnel_host", "TunnelHost"),
    ("tunnel_port", "TunnelPort"),
    ("tunnel_username", "TunnelUsername"),
    ("tunnel_authentication", "TunnelAuthentication"),
    ("tunnel_identity_file", "TunnelIdentityFile"),
    ("tunnel_keep_alive", "TunnelKeepAlive"),
    ("tunnel_prompt_password", "TunnelPromptPassword"),
    ("shared", "Shared"),
    ("shared_username", "SharedUsername"),
    ("kerberos_conn", "KerberosAuthentication"),
    ("passexec_cmd", "PasswordExecCommand"),
    ("passexec_expiration", "PasswordExecExpiration"),
    ("connection_params", "ConnectionParameters"),
    ("tags", "Tags"),
    ("post_connection_sql", "PostConnectionSQL"),
    ("prepare_threshold", "PrepareThreshold"),
];

/// Columns that are pgAdmin's bookkeeping, or secrets that are never selected.
const DATABASE_SKIPPED: &[&str] = &[
    "id",
    "user_id",
    "servergroup_id",
    "password",
    "tunnel_password",
    "save_password",
    "discovery_id",
    "cloud_status",
    "is_adhoc",
    "passexec_name",
];

/// Columns holding JSON text.
const JSON_COLUMNS: &[&str] = &["connection_params", "tags"];

fn read_database(path: &Path) -> Result<Vec<ImportedConnection>, PgadminError> {
    use rusqlite::{OpenFlags, types::ValueRef};
    let db = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| unreadable(path, e))?;
    let _ = db.busy_timeout(std::time::Duration::from_secs(2));
    let not_pgadmin = |_| unreadable(path, "not pgAdmin's database");
    let columns: Vec<String> = db
        .prepare("SELECT name FROM pragma_table_info('server')")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()
        })
        .map_err(not_pgadmin)?;
    if !columns.iter().any(|c| c == "name") {
        return Err(unreadable(path, "not pgAdmin's database (no servers)"));
    }
    let has = |c: &str| columns.iter().any(|x| x == c);
    // Only names from the table's own columns are written into the statement, quoted.
    let selected: Vec<&String> = columns
        .iter()
        .filter(|c| !DATABASE_SKIPPED.contains(&c.as_str()))
        .collect();
    let mut fields: Vec<String> = selected
        .iter()
        .map(|c| format!("s.\"{}\"", c.replace('"', "\"\"")))
        .collect();
    // Whether a password was saved, never the password.
    fields.push(if has("save_password") && has("password") {
        "(s.save_password = 1 AND s.password IS NOT NULL AND s.password <> '')".into()
    } else {
        "0".into()
    });
    fields.push(if has("tunnel_password") {
        "(s.tunnel_password IS NOT NULL AND s.tunnel_password <> '')".into()
    } else {
        "0".into()
    });
    let group = if has("servergroup_id") {
        "(SELECT g.name FROM servergroup g WHERE g.id = s.servergroup_id)"
    } else {
        "NULL"
    };
    fields.push(group.into());
    let id = if has("id") { "s.id" } else { "s.rowid" };
    fields.push(id.into());
    // Ad hoc servers are the query tool's one-off connections, not in pgAdmin's tree.
    let filter = if has("is_adhoc") {
        "WHERE COALESCE(s.is_adhoc, 0) = 0"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {} FROM server s {filter} ORDER BY {id} LIMIT {}",
        fields.join(", "),
        MAX_SERVERS
    );
    let mut statement = db.prepare(&sql).map_err(not_pgadmin)?;
    let count = selected.len();
    let rows = statement
        .query_map([], |row| {
            let mut server = Map::new();
            for (i, column) in selected.iter().enumerate() {
                let value = match row.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => Value::from(n),
                    ValueRef::Real(f) => Value::from(f),
                    ValueRef::Text(t) => {
                        let text = String::from_utf8_lossy(t).into_owned();
                        if JSON_COLUMNS.contains(&column.as_str()) {
                            serde_json::from_str(&text).unwrap_or(Value::String(text))
                        } else {
                            Value::String(text)
                        }
                    }
                    ValueRef::Blob(_) => Value::String("(binary)".into()),
                };
                let key = DATABASE_KEYS
                    .iter()
                    .find(|(c, _)| *c == column.as_str())
                    .map(|(_, k)| (*k).to_string())
                    .unwrap_or_else(|| (*column).clone());
                server.insert(key, value);
            }
            let saved_password: bool = row.get::<_, Option<i64>>(count)?.unwrap_or(0) != 0;
            let saved_tunnel_password: bool =
                row.get::<_, Option<i64>>(count + 1)?.unwrap_or(0) != 0;
            if let Some(group) = row.get::<_, Option<String>>(count + 2)? {
                server.insert("Group".into(), Value::String(group));
            }
            let id: i64 = row.get(count + 3)?;
            Ok((
                id.to_string(),
                server,
                Origin::Database {
                    saved_password,
                    saved_tunnel_password,
                },
            ))
        })
        .map_err(|e| unreadable(path, e))?;
    let mut connections = Vec::new();
    for row in rows {
        let (id, server, origin) = row.map_err(|e| unreadable(path, e))?;
        connections.push(connection_of(&id, &server, origin));
    }
    Ok(connections)
}

/// One server's settings, read leniently: pgAdmin writes ports as numbers or text, and
/// flags as booleans or 0/1.
struct Server<'a>(&'a Map<String, Value>);

impl Server<'_> {
    fn text(&self, key: &str) -> String {
        text_of(self.0.get(key))
    }

    fn number(&self, key: &str) -> Option<i64> {
        number_of(self.0.get(key))
    }

    fn flag(&self, key: &str) -> bool {
        flag_of(self.0.get(key))
    }
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn number_of(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn flag_of(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().is_some_and(|n| n != 0),
        Some(Value::String(s)) => matches!(s.trim(), "1" | "true" | "True"),
        _ => false,
    }
}

/// A value that says nothing: missing, empty, zero, false.
fn is_unset(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn port_of(value: Option<i64>) -> Option<u16> {
    value.and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0)
}

/// The keys this reader maps, or that say nothing on their own: anything else with a value is
/// listed as not imported.
const KNOWN: &[&str] = &[
    "Name",
    "Group",
    "Host",
    "Port",
    "MaintenanceDB",
    "Username",
    "Comment",
    "Role",
    "DBRestriction",
    "DBRestrictionType",
    "BGColor",
    "FGColor",
    "Service",
    "UseSSHTunnel",
    "TunnelHost",
    "TunnelPort",
    "TunnelUsername",
    "TunnelAuthentication",
    "TunnelIdentityFile",
    "TunnelKeepAlive",
    "TunnelPromptPassword",
    "Shared",
    "SharedUsername",
    "KerberosAuthentication",
    "PasswordExecCommand",
    "PasswordExecExpiration",
    "ConnectionParameters",
    "Tags",
    "PostConnectionSQL",
    // Older exports: the TLS settings and the password file outside ConnectionParameters.
    "SSLMode",
    "SSLRootCert",
    "SSLCert",
    "SSLKey",
    "PassFile",
];

/// The connection parameters this reader maps; any other one is listed as not imported.
const KNOWN_PARAMETERS: &[&str] = &[
    "sslmode",
    "sslrootcert",
    "sslcert",
    "sslkey",
    "connect_timeout",
    "passfile",
];

/// libpq's `sslmode`. `allow` has no equivalent: SpelaionDB's default (prefer) is kept.
fn tls_mode(mode: &str, unmapped: &mut Vec<String>) -> Option<TlsModeConfig> {
    match mode.to_ascii_lowercase().as_str() {
        "" | "prefer" => None,
        "disable" => Some(TlsModeConfig::Disable),
        "require" => Some(TlsModeConfig::Require),
        "verify-ca" => Some(TlsModeConfig::VerifyCa),
        "verify-full" => Some(TlsModeConfig::VerifyFull),
        other => {
            unmapped.push(format!(
                "TLS: SSL mode {} (SpelaionDB's default, prefer, is kept)",
                shown_value(other)
            ));
            None
        }
    }
}

/// A value for the preview: without control characters, at most 80 of them.
fn shown_value(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).take(80).collect()
}

const EXPORT_PASSWORD: &str =
    "Password: pgAdmin does not export passwords; it is asked when connecting";
const SAVED_PASSWORD: &str = "Password: pgAdmin saved one, encrypted with its master password (or a key of its own), which SpelaionDB does not read; it is asked when connecting";

fn connection_of(id: &str, map: &Map<String, Value>, origin: Origin) -> ImportedConnection {
    let server = Server(map);
    let params = map
        .get("ConnectionParameters")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let param = |key: &str| text_of(params.get(key));
    let mut unmapped = Vec::new();

    let host = server.text("Host");
    let service = server.text("Service");
    let engine = if host.is_empty() {
        Err(if service.is_empty() {
            "a server without a host name".to_string()
        } else {
            format!(
                "a pg_service.conf service ({}), which SpelaionDB does not read",
                shown_value(&service)
            )
        })
    } else if host.starts_with('/') || host.starts_with('@') {
        Err(format!(
            "a local socket connection ({}), which SpelaionDB does not open",
            shown_value(&host)
        ))
    } else {
        Ok(Engine::Postgres)
    };
    let importable = engine.is_ok();
    if importable && !service.is_empty() {
        unmapped.push(format!(
            "Service: {} (pg_service.conf is not read)",
            shown_value(&service)
        ));
    }

    // TLS: in ConnectionParameters; older exports wrote them as their own keys.
    let pick = |param_key: &str, old_key: &str| {
        let value = param(param_key);
        if value.is_empty() {
            server.text(old_key)
        } else {
            value
        }
    };
    let mode = tls_mode(&pick("sslmode", "SSLMode"), &mut unmapped);
    let (ca, cert, key) = (
        pick("sslrootcert", "SSLRootCert"),
        pick("sslcert", "SSLCert"),
        pick("sslkey", "SSLKey"),
    );
    for path in [&ca, &cert, &key] {
        if path.contains("<STORAGE_DIR>") {
            unmapped.push(format!(
                "TLS: {} is in pgAdmin's storage folder: fix the path after importing",
                shown_value(path)
            ));
        }
    }
    let has_files = !(ca.is_empty() && cert.is_empty() && key.is_empty());
    let tls = match (importable, mode, has_files) {
        (false, _, _) | (true, None, false) => None,
        (true, mode, _) => Some(ImportedTls {
            mode: mode.unwrap_or_default(),
            ca,
            cert,
            key,
        }),
    };

    let ssh = server.flag("UseSSHTunnel").then(|| {
        let (host, port) = (
            server.text("TunnelHost"),
            port_of(server.number("TunnelPort")),
        );
        let identity = server.number("TunnelAuthentication") == Some(1);
        let key_path = server.text("TunnelIdentityFile");
        if identity {
            unmapped.push(
                "SSH: if the private key has a passphrase, it is asked when connecting".into(),
            );
        } else {
            match origin {
                Origin::Database {
                    saved_tunnel_password: true,
                    ..
                } => unmapped.push("SSH: pgAdmin saved the SSH password, encrypted, which SpelaionDB does not read; it is asked when connecting".into()),
                Origin::Database { .. } => {}
                Origin::Export => unmapped.push(
                    "SSH: pgAdmin does not export the SSH password; it is asked when connecting"
                        .into(),
                ),
            }
        }
        let keepalive = server
            .number("TunnelKeepAlive")
            .and_then(|s| u32::try_from(s).ok())
            .filter(|s| *s > 0);
        ImportedSsh {
            host,
            port,
            user: server.text("TunnelUsername"),
            auth: if identity { SshAuth::Key } else { SshAuth::Password },
            key_path: if identity { key_path } else { String::new() },
            password: None,
            keepalive_interval: keepalive,
            keepalive_count: None,
            connect_timeout: None,
            local_port: None,
        }
    });

    if importable {
        match origin {
            Origin::Export => unmapped.push(EXPORT_PASSWORD.into()),
            Origin::Database {
                saved_password: true,
                ..
            } => unmapped.push(SAVED_PASSWORD.into()),
            Origin::Database { .. } => {}
        }
    }
    if server.flag("KerberosAuthentication") {
        unmapped.push("Kerberos authentication (SpelaionDB signs in with a password)".into());
    }
    if !server.text("PasswordExecCommand").is_empty() {
        unmapped
            .push("Password exec command (not run; the password is asked when connecting)".into());
    }
    let passfile = pick("passfile", "PassFile");
    if !passfile.is_empty() {
        unmapped.push(format!(
            "Password file: {} (not read; the password is asked when connecting)",
            shown_value(&passfile)
        ));
    }
    let role = server.text("Role");
    if !role.is_empty() {
        unmapped.push(format!(
            "Role: {} (add SET ROLE to the connection's session setup)",
            shown_value(&role)
        ));
    }
    let post_sql = server.text("PostConnectionSQL");
    if !post_sql.is_empty() {
        unmapped.push(format!(
            "Post-connection SQL: {} (add it to the connection's session setup)",
            shown_value(&post_sql)
        ));
    }
    let restriction = server.text("DBRestriction");
    if !restriction.is_empty() {
        let kind = server.text("DBRestrictionType");
        let kind = if kind.is_empty() {
            "databases".to_string()
        } else {
            shown_value(&kind)
        };
        unmapped.push(format!(
            "Shown {kind} limited to: {}",
            shown_value(&restriction)
        ));
    }
    for (key, what) in [("BGColor", "background"), ("FGColor", "foreground")] {
        let colour = server.text(key);
        if !colour.is_empty() {
            unmapped.push(format!("Colour: {what} {}", shown_value(&colour)));
        }
    }
    if server.flag("Shared") {
        let user = server.text("SharedUsername");
        unmapped.push(if user.is_empty() {
            "Shared with other pgAdmin users".to_string()
        } else {
            format!(
                "Shared with other pgAdmin users (as {})",
                shown_value(&user)
            )
        });
    }
    if let Some(timeout) = number_of(params.get("connect_timeout"))
        && timeout != DEFAULT_CONNECT_TIMEOUT
        && timeout > 0
    {
        unmapped.push(format!("Connection timeout: {timeout} s"));
    }
    for (name, value) in &params {
        if KNOWN_PARAMETERS.contains(&name.as_str()) || is_unset(value) {
            continue;
        }
        unmapped.push(format!(
            "Connection parameter: {}",
            shown(name, &text_of(Some(value)))
        ));
    }

    let mut tags = Vec::new();
    let mut tag_colours = Vec::new();
    for tag in map
        .get("Tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = text_of(tag.get("text"));
        if text.is_empty() {
            continue;
        }
        let colour = text_of(tag.get("color"));
        if !colour.is_empty() {
            tag_colours.push(format!("{} {}", shown_value(&text), shown_value(&colour)));
        }
        tags.push(text);
    }
    if !tag_colours.is_empty() {
        unmapped.push(format!("Tag colours: {}", tag_colours.join(", ")));
    }

    let mut others: Vec<(&String, &Value)> = map
        .iter()
        .filter(|(key, value)| !KNOWN.contains(&key.as_str()) && !is_unset(value))
        .collect();
    others.sort_by(|a, b| a.0.cmp(b.0));
    for (key, value) in others {
        let text = match value {
            Value::Array(_) | Value::Object(_) => String::new(),
            other => text_of(Some(other)),
        };
        unmapped.push(format!("Option: {}", shown(key, &text)));
    }

    ImportedConnection {
        id: id.to_string(),
        name: server.text("Name"),
        description: map
            .get("Comment")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tags,
        folder: server.text("Group"),
        engine,
        host: if importable { host } else { String::new() },
        port: if importable {
            port_of(server.number("Port"))
        } else {
            None
        },
        database: server.text("MaintenanceDB"),
        user: server.text("Username"),
        password: None,
        stored_password: None,
        env: String::new(),
        read_only: false,
        ssh: if importable { ssh } else { None },
        proxy: None,
        tls,
        unmapped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_modes_follow_libpq() {
        let mut unmapped = Vec::new();
        assert_eq!(tls_mode("", &mut unmapped), None);
        assert_eq!(tls_mode("prefer", &mut unmapped), None);
        assert_eq!(
            tls_mode("disable", &mut unmapped),
            Some(TlsModeConfig::Disable)
        );
        assert_eq!(
            tls_mode("require", &mut unmapped),
            Some(TlsModeConfig::Require)
        );
        assert_eq!(
            tls_mode("VERIFY-CA", &mut unmapped),
            Some(TlsModeConfig::VerifyCa)
        );
        assert_eq!(
            tls_mode("verify-full", &mut unmapped),
            Some(TlsModeConfig::VerifyFull)
        );
        assert!(unmapped.is_empty());
        assert_eq!(tls_mode("allow", &mut unmapped), None);
        assert_eq!(
            unmapped,
            ["TLS: SSL mode allow (SpelaionDB's default, prefer, is kept)"]
        );
    }

    #[test]
    fn values_are_read_leniently() {
        let map: Map<String, Value> = serde_json::from_str(
            r#"{"a": "2221", "b": 2221, "c": true, "d": 1, "e": "x", "f": null}"#,
        )
        .unwrap();
        let s = Server(&map);
        assert_eq!(s.number("a"), Some(2221));
        assert_eq!(s.number("b"), Some(2221));
        assert!(s.flag("c") && s.flag("d") && !s.flag("e") && !s.flag("f"));
        assert_eq!(s.text("b"), "2221");
        assert_eq!(s.text("f"), "");
        assert!(is_unset(&Value::from(0)) && is_unset(&Value::Bool(false)));
        assert!(!is_unset(&Value::from("x")));
    }
}
