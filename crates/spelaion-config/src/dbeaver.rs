//! Connections imported from DBeaver: its `data-sources*.json` files and, when DBeaver saved
//! them, the user names and passwords of `credentials-config.json`. DBeaver encrypts that
//! file with AES-128-CBC and a key built into DBeaver (it obscures, it does not protect), so
//! we can read what DBeaver itself reads. Decrypted passwords only live as [`Secret`]s on
//! their way to the keychain: they are never shown, logged or written to a file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aes::Aes128;
use cbc::Decryptor;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use serde_json::Value as Json;
use spelaion_driver_api::Secret;

use crate::draft::{ConnectionDraft, PasswordChoice, slug};
use crate::load::Source;
use crate::model::{ConnectionFile, Engine, ProxyType, SshAuth, TlsModeConfig, TunnelFile};
use crate::tunnel_draft::{HopDraft, ProxyDraft, TunnelDraft, TunnelDraftOptions, TunnelPasswords};

/// The key DBeaver ships with to obscure `credentials-config.json`.
const CREDENTIALS_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

/// One DBeaver connection, as far as SpelaionDB can use it.
#[derive(Debug, Clone)]
pub struct DbeaverConnection {
    /// DBeaver's own identifier (`mysql8-18f…`), to pick connections to import.
    pub id: String,
    pub name: String,
    /// DBeaver folder, `a/b` for nested ones.
    pub folder: String,
    /// The engine, or why SpelaionDB cannot open it (yet).
    pub engine: Result<Engine, String>,
    pub host: String,
    pub port: Option<u16>,
    /// Database name; for SQLite, the file path.
    pub database: String,
    pub user: String,
    pub password: Option<Secret>,
    /// DBeaver's connection type (`dev`, `test`, `prod` or a custom one).
    pub env: String,
    pub ssh: Option<DbeaverSsh>,
    /// DBeaver's SOCKS proxy handler (`socks_proxy`), alone or in front of the SSH tunnel.
    /// DBeaver has no per-connection HTTP proxy: its HTTP proxy is a global preference.
    pub proxy: Option<DbeaverProxy>,
}

#[derive(Debug, Clone)]
pub struct DbeaverProxy {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password: Option<Secret>,
}

impl DbeaverConnection {
    /// The name its imported network profile gets: one per SSH bastion and user (and proxy,
    /// if any), or per proxy alone, so connections that share one in DBeaver share it here.
    pub fn tunnel_name(&self) -> Option<String> {
        let proxy = self
            .proxy
            .as_ref()
            .map(|p| format!("{} {}", p.host, p.port.unwrap_or(1080)));
        match (&self.ssh, proxy) {
            (Some(ssh), None) => Some(slug(&format!("{} {}", ssh.user, ssh.host))),
            (Some(ssh), Some(proxy)) => {
                Some(slug(&format!("{} {} via {proxy}", ssh.user, ssh.host)))
            }
            (None, Some(proxy)) => Some(slug(&format!("socks {proxy}"))),
            (None, None) => None,
        }
    }
}

/// The SSH tunnel of a DBeaver connection.
#[derive(Debug, Clone)]
pub struct DbeaverSsh {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub auth: SshAuth,
    pub key_path: String,
    pub password: Option<Secret>,
    /// Seconds (DBeaver keeps milliseconds); `None` where DBeaver left it unset or 0.
    pub keepalive_interval: Option<u32>,
    pub keepalive_count: Option<u32>,
    /// Seconds (DBeaver keeps milliseconds).
    pub connect_timeout: Option<u32>,
    /// A fixed local port for the tunnel; `None` where DBeaver picks a free one.
    pub local_port: Option<u16>,
}

#[derive(Debug, thiserror::Error)]
pub enum DbeaverError {
    #[error("no DBeaver configuration (.dbeaver/data-sources.json) under {0}")]
    NotFound(PathBuf),
    #[error("could not read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
}

/// Where DBeaver keeps its workspace on this system (it may not exist).
pub fn dbeaver_workspace() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
    };
    base.map(|base| base.join("DBeaverData").join("workspace6"))
}

/// Every connection under `dir`: a DBeaver workspace (all its projects), one project, or a
/// `.dbeaver` folder.
pub fn read_dbeaver(dir: &Path) -> Result<Vec<DbeaverConnection>, DbeaverError> {
    let folders = dbeaver_folders(dir);
    if folders.is_empty() {
        return Err(DbeaverError::NotFound(dir.to_path_buf()));
    }
    let mut out = Vec::new();
    for folder in folders {
        let credentials = read_credentials(&folder.join("credentials-config.json"))?;
        let mut files: Vec<PathBuf> = std::fs::read_dir(&folder)
            .map_err(|e| unreadable(&folder, e))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("data-sources") && n.ends_with(".json"))
            })
            .collect();
        files.sort();
        for file in files {
            let text = std::fs::read_to_string(&file).map_err(|e| unreadable(&file, e))?;
            let json: Json = serde_json::from_str(&text).map_err(|e| unreadable(&file, e))?;
            let Some(connections) = json.get("connections").and_then(Json::as_object) else {
                continue;
            };
            for (id, connection) in connections {
                out.push(connection_of(id, connection, credentials.get(id)));
            }
        }
    }
    Ok(out)
}

fn unreadable(path: &Path, e: impl std::fmt::Display) -> DbeaverError {
    DbeaverError::Unreadable {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// The `.dbeaver` folders under `dir`, wherever it points in the workspace.
fn dbeaver_folders(dir: &Path) -> Vec<PathBuf> {
    let has_sources = |d: &Path| d.join("data-sources.json").is_file();
    if dir.file_name().is_some_and(|n| n == ".dbeaver") && has_sources(dir) {
        return vec![dir.to_path_buf()];
    }
    if has_sources(&dir.join(".dbeaver")) {
        return vec![dir.join(".dbeaver")];
    }
    let mut projects: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path().join(".dbeaver"))
        .filter(|d| has_sources(d))
        .collect();
    projects.sort();
    projects
}

/// DBeaver's saved credentials by connection: `{"<id>": {"#connection": {"user", "password"},
/// "network/ssh_tunnel": {…}}}`. A missing file means nothing was saved.
fn read_credentials(path: &Path) -> Result<HashMap<String, Json>, DbeaverError> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(HashMap::new());
    };
    let plain =
        decrypt(&bytes).ok_or_else(|| unreadable(path, "not a DBeaver credentials file"))?;
    let json: HashMap<String, Json> =
        serde_json::from_slice(&plain).map_err(|e| unreadable(path, e))?;
    Ok(json)
}

/// The first 16 bytes are the IV; the rest, AES-128-CBC with PKCS#7 padding.
fn decrypt(bytes: &[u8]) -> Option<Vec<u8>> {
    let (iv, data) = (bytes.get(..16)?, bytes.get(16..)?);
    let mut buffer = data.to_vec();
    let plain = Decryptor::<Aes128>::new_from_slices(&CREDENTIALS_KEY, iv)
        .ok()?
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .ok()?;
    Some(plain.to_vec())
}

fn text(value: Option<&Json>) -> String {
    match value {
        Some(Json::String(s)) => s.trim().to_string(),
        Some(Json::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn port(value: Option<&Json>) -> Option<u16> {
    text(value).parse().ok()
}

/// A positive number; DBeaver writes 0 for "not set".
fn positive(value: Option<&Json>) -> Option<u64> {
    text(value).parse().ok().filter(|n| *n > 0)
}

/// DBeaver's milliseconds as whole seconds, rounded up so a short interval stays on.
fn seconds(value: Option<&Json>) -> Option<u32> {
    positive(value).map(|ms| u32::try_from(ms.div_ceil(1000)).unwrap_or(u32::MAX))
}

fn connection_of(id: &str, json: &Json, credentials: Option<&Json>) -> DbeaverConnection {
    let config = json.get("configuration").unwrap_or(&Json::Null);
    let provider = text(json.get("provider")).to_lowercase();
    let driver = text(json.get("driver")).to_lowercase();
    let engine = match (provider.as_str(), driver.as_str()) {
        ("postgresql", _) => Ok(Engine::Postgres),
        ("mysql", d) if d.contains("maria") => Ok(Engine::Mariadb),
        ("mysql", _) => Ok(Engine::Mysql),
        ("sqlite", _) | ("generic", "sqlite_jdbc") => Ok(Engine::Sqlite),
        ("duckdb", _) | ("generic", "duckdb_jdbc") => Ok(Engine::Duckdb),
        ("sqlserver", _) | ("mssql", _) => Ok(Engine::Sqlserver),
        (p, d) => Err(format!("{p}/{d} is not supported yet")),
    };
    let saved = credentials.and_then(|c| c.get("#connection"));
    let mut database = text(config.get("database"));
    if engine.as_ref().is_ok_and(|e| e.is_file()) && database.is_empty() {
        let url = text(config.get("url"));
        database = url
            .strip_prefix("jdbc:sqlite:")
            .or_else(|| url.strip_prefix("jdbc:duckdb:"))
            .unwrap_or_default()
            .to_string();
    }
    let user = match text(saved.and_then(|s| s.get("user"))) {
        user if user.is_empty() => text(config.get("user")),
        user => user,
    };
    let password = saved
        .and_then(|s| s.get("password"))
        .and_then(Json::as_str)
        .filter(|p| !p.is_empty())
        .map(Secret::new);
    DbeaverConnection {
        id: id.to_string(),
        name: text(json.get("name")),
        folder: text(json.get("folder")),
        engine,
        host: text(config.get("host")),
        port: port(config.get("port")),
        database,
        user,
        password,
        env: text(config.get("type")),
        ssh: ssh_of(config, credentials),
        proxy: proxy_of(config, credentials),
    }
}

fn proxy_of(config: &Json, credentials: Option<&Json>) -> Option<DbeaverProxy> {
    let handler = config.get("handlers")?.get("socks_proxy")?;
    if handler.get("enabled").and_then(Json::as_bool) != Some(true) {
        return None;
    }
    let properties = handler.get("properties").unwrap_or(&Json::Null);
    let host = text(properties.get("socks-host"));
    if host.is_empty() {
        return None;
    }
    let saved = credentials.and_then(|c| c.get("network/socks_proxy"));
    let user = match text(saved.and_then(|s| s.get("user"))) {
        user if user.is_empty() => text(properties.get("socks-user")),
        user => user,
    };
    Some(DbeaverProxy {
        host,
        port: port(properties.get("socks-port")),
        user,
        password: saved
            .and_then(|s| s.get("password"))
            .and_then(Json::as_str)
            .filter(|p| !p.is_empty())
            .map(Secret::new),
    })
}

fn ssh_of(config: &Json, credentials: Option<&Json>) -> Option<DbeaverSsh> {
    let handler = config.get("handlers")?.get("ssh_tunnel")?;
    if handler.get("enabled").and_then(Json::as_bool) != Some(true) {
        return None;
    }
    let properties = handler.get("properties").unwrap_or(&Json::Null);
    let saved = credentials.and_then(|c| c.get("network/ssh_tunnel"));
    let user = match text(saved.and_then(|s| s.get("user"))) {
        user if user.is_empty() => text(properties.get("user")),
        user => user,
    };
    let auth = match text(properties.get("authType")).to_uppercase().as_str() {
        "PUBLIC_KEY" => SshAuth::Key,
        "AGENT" => SshAuth::Agent,
        _ => SshAuth::Password,
    };
    Some(DbeaverSsh {
        host: text(properties.get("host")),
        port: port(properties.get("port")),
        user,
        auth,
        key_path: text(properties.get("keyPath")),
        password: saved
            .and_then(|s| s.get("password"))
            .and_then(Json::as_str)
            .filter(|p| !p.is_empty())
            .map(Secret::new),
        keepalive_interval: seconds(properties.get("aliveInterval"))
            .map(|s| s.min(TunnelFile::MAX_KEEPALIVE_INTERVAL)),
        keepalive_count: positive(properties.get("aliveCount")).map(|n| {
            u32::try_from(n)
                .unwrap_or(u32::MAX)
                .min(TunnelFile::MAX_KEEPALIVE_COUNT)
        }),
        connect_timeout: seconds(properties.get("sshConnectTimeout"))
            .map(|s| s.min(TunnelFile::MAX_CONNECT_TIMEOUT)),
        local_port: port(properties.get("localPort")).filter(|p| *p > 0),
    })
}

/// What SpelaionDB will create for a connection: the connection, its tunnel (if any) and
/// the secrets that go to the keychain with them.
pub struct ImportPlan {
    pub connection: ConnectionDraft,
    pub password: Option<Secret>,
    pub tunnel: Option<(TunnelDraft, TunnelPasswords)>,
}

/// The drafts for one DBeaver connection. With `save_passwords`, the passwords DBeaver saved
/// go to the keychain; otherwise (or when DBeaver saved none) they are asked when connecting.
pub fn import_plan(
    connection: &DbeaverConnection,
    engine: Engine,
    source: Source,
    save_passwords: bool,
    tunnel_name: &str,
) -> ImportPlan {
    let keep = |secret: &Option<Secret>| {
        save_passwords
            .then(|| secret.as_ref().map(|s| Secret::new(s.expose())))
            .flatten()
    };
    let choice = |secret: &Option<Secret>| {
        if keep(secret).is_some() {
            PasswordChoice::Keychain
        } else {
            PasswordChoice::Prompt
        }
    };
    let proxy = connection.proxy.as_ref().map(|proxy| ProxyDraft {
        kind: ProxyType::Socks5,
        host: proxy.host.clone(),
        port: Some(proxy.port.unwrap_or(1080)),
        user: proxy.user.clone(),
        password: if proxy.user.is_empty() {
            PasswordChoice::None
        } else {
            choice(&proxy.password)
        },
    });
    let proxy_password = connection
        .proxy
        .as_ref()
        .and_then(|proxy| keep(&proxy.password));
    let tunnel = match (&connection.ssh, proxy) {
        (None, None) => None,
        (Some(ssh), proxy) => {
            let password = match ssh.auth {
                SshAuth::Password | SshAuth::KeyboardInteractive => choice(&ssh.password),
                SshAuth::Agent | SshAuth::Key => PasswordChoice::None,
            };
            Some((
                TunnelDraft {
                    source,
                    name: tunnel_name.to_string(),
                    hops: vec![HopDraft {
                        host: ssh.host.clone(),
                        port: ssh.port,
                        user: ssh.user.clone(),
                        auth: vec![ssh.auth],
                        identity_file: ssh.key_path.clone(),
                        password,
                    }],
                    options: TunnelDraftOptions {
                        keepalive_interval: ssh.keepalive_interval,
                        keepalive_count: ssh.keepalive_count,
                        connect_timeout: ssh.connect_timeout,
                    },
                    proxy,
                },
                TunnelPasswords {
                    hops: vec![keep(&ssh.password)],
                    proxy: proxy_password,
                },
            ))
        }
        // The proxy alone reaches the database (ADR-027).
        (None, Some(proxy)) => Some((
            TunnelDraft {
                source,
                name: tunnel_name.to_string(),
                hops: Vec::new(),
                options: TunnelDraftOptions::default(),
                proxy: Some(proxy),
            },
            TunnelPasswords {
                hops: Vec::new(),
                proxy: proxy_password,
            },
        )),
    };
    let sqlite = engine.is_file();
    ImportPlan {
        connection: ConnectionDraft {
            source,
            folder: connection.folder.to_lowercase(),
            name: connection.name.clone(),
            engine,
            env: connection.env.clone(),
            host: if sqlite {
                String::new()
            } else {
                connection.host.clone()
            },
            port: if sqlite { None } else { connection.port },
            database: connection.database.clone(),
            user: if sqlite {
                String::new()
            } else {
                connection.user.clone()
            },
            password: if sqlite {
                PasswordChoice::None
            } else {
                choice(&connection.password)
            },
            tls_mode: TlsModeConfig::default(),
            tls_ca: String::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            tunnel: if tunnel.is_some() {
                tunnel_name.to_string()
            } else {
                String::new()
            },
            local_port: connection.ssh.as_ref().and_then(|ssh| ssh.local_port),
            kubernetes: None,
            sqlite3: String::new(),
            read_only: false,
            // Like a connection made in the form: idle sessions are kept alive (ADR-028).
            keepalive: (!sqlite).then_some(ConnectionFile::SUGGESTED_KEEPALIVE),
            mcp_exposed: false,
            mcp_writes: false,
        },
        password: if sqlite {
            None
        } else {
            keep(&connection.password)
        },
        tunnel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::Encryptor;
    use cbc::cipher::BlockEncryptMut;

    /// A DBeaver workspace with one project, like the one DBeaver writes.
    fn workspace(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("spelaion-dbeaver-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("General").join(".dbeaver");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("data-sources.json"),
            r#"{
              "folders": {"Prod": {}},
              "connections": {
                "postgres-jdbc-1": {
                  "provider": "postgresql", "driver": "postgres-jdbc", "name": "Orders PG",
                  "folder": "Prod", "save-password": true,
                  "configuration": {
                    "host": "db.internal", "port": "5432", "database": "orders", "type": "prod",
                    "handlers": {"ssh_tunnel": {"type": "TUNNEL", "enabled": true,
                      "properties": {"host": "bastion.example.com", "port": 22, "authType": "PASSWORD",
                        "aliveInterval": "29500", "aliveCount": "5",
                        "sshConnectTimeout": "20000", "localPort": "15432"}}}
                  }
                },
                "mariaDB-2": {
                  "provider": "mysql", "driver": "mariaDB", "name": "Shop Maria",
                  "configuration": {"host": "localhost", "port": "3306", "database": "shop", "type": "dev"}
                },
                "sqlite_jdbc-3": {
                  "provider": "sqlite", "driver": "sqlite_jdbc", "name": "Local file",
                  "configuration": {"database": "C:\\data\\local.db", "type": "dev"}
                },
                "oracle_thin-4": {
                  "provider": "oracle", "driver": "oracle_thin", "name": "Legacy",
                  "configuration": {"host": "ora", "port": "1521"}
                },
                "duckdb_jdbc-5": {
                  "provider": "duckdb", "driver": "duckdb_jdbc", "name": "Analytics",
                  "configuration": {"url": "jdbc:duckdb:C:\\data\\sales.duckdb", "type": "dev"}
                },
                "mysql8-6": {
                  "provider": "mysql", "driver": "mysql8", "name": "Behind SOCKS", "save-password": true,
                  "configuration": {
                    "host": "localhost", "port": "3306", "type": "dev",
                    "handlers": {"socks_proxy": {"type": "PROXY", "enabled": true, "save-password": true,
                      "properties": {"socks-host": "proxy.corp", "socks-port": 1080}}}
                  }
                }
              }
            }"#,
        )
        .unwrap();
        let credentials =
            br##"{"postgres-jdbc-1": {"#connection": {"user": "app", "password": "s3cret"},
            "network/ssh_tunnel": {"user": "jump", "password": "hop-pass"}},
            "mariaDB-2": {"#connection": {"user": "root"}},
            "mysql8-6": {"network/socks_proxy": {"user": "proxy-user", "password": "proxy-secret"}}}"##;
        let iv = [7u8; 16];
        let mut buffer = credentials.to_vec();
        buffer.resize(credentials.len() + 16, 0);
        let encrypted = Encryptor::<Aes128>::new_from_slices(&CREDENTIALS_KEY, &iv)
            .unwrap()
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, credentials.len())
            .unwrap()
            .to_vec();
        std::fs::write(
            dir.join("credentials-config.json"),
            [iv.as_slice(), &encrypted].concat(),
        )
        .unwrap();
        root
    }

    #[test]
    fn connections_and_saved_credentials_are_read() {
        let root = workspace("read");
        let found = read_dbeaver(&root).unwrap();
        let by_id = |id: &str| found.iter().find(|c| c.id == id).unwrap();
        let pg = by_id("postgres-jdbc-1");
        assert_eq!(pg.engine, Ok(Engine::Postgres));
        assert_eq!(
            (pg.host.as_str(), pg.port, pg.database.as_str()),
            ("db.internal", Some(5432), "orders")
        );
        assert_eq!(
            (pg.user.as_str(), pg.env.as_str(), pg.folder.as_str()),
            ("app", "prod", "Prod")
        );
        assert_eq!(pg.password.as_ref().map(Secret::expose), Some("s3cret"));
        let ssh = pg.ssh.as_ref().unwrap();
        assert_eq!(
            (ssh.host.as_str(), ssh.port, ssh.user.as_str()),
            ("bastion.example.com", Some(22), "jump")
        );
        assert_eq!(ssh.auth, SshAuth::Password);
        assert_eq!(ssh.password.as_ref().map(Secret::expose), Some("hop-pass"));
        assert_eq!(
            (ssh.keepalive_interval, ssh.keepalive_count),
            (Some(30), Some(5))
        );
        assert_eq!(ssh.connect_timeout, Some(20));
        assert_eq!(ssh.local_port, Some(15432));
        let maria = by_id("mariaDB-2");
        assert_eq!(
            (maria.engine.clone(), maria.user.as_str()),
            (Ok(Engine::Mariadb), "root")
        );
        assert!(maria.password.is_none() && maria.ssh.is_none());
        assert_eq!(by_id("sqlite_jdbc-3").database, "C:\\data\\local.db");
        assert!(by_id("oracle_thin-4").engine.is_err());
        let duck = by_id("duckdb_jdbc-5");
        assert_eq!(
            (duck.engine.clone(), duck.database.as_str()),
            (Ok(Engine::Duckdb), "C:\\data\\sales.duckdb")
        );
        // The same from the project folder or the `.dbeaver` folder itself.
        assert_eq!(read_dbeaver(&root.join("General")).unwrap().len(), 6);
        assert_eq!(
            read_dbeaver(&root.join("General").join(".dbeaver"))
                .unwrap()
                .len(),
            6
        );
        assert!(matches!(
            read_dbeaver(&root.join("nothing")),
            Err(DbeaverError::NotFound(_))
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn socks_proxies_become_network_profiles() {
        let root = workspace("socks");
        let found = read_dbeaver(&root).unwrap();
        let socks = found.iter().find(|c| c.id == "mysql8-6").unwrap();
        let proxy = socks.proxy.as_ref().expect("the SOCKS handler");
        assert_eq!(
            (proxy.host.as_str(), proxy.port, proxy.user.as_str()),
            ("proxy.corp", Some(1080), "proxy-user")
        );
        assert_eq!(
            proxy.password.as_ref().map(Secret::expose),
            Some("proxy-secret")
        );
        assert!(socks.ssh.is_none());
        let name = socks.tunnel_name().unwrap();
        assert_eq!(name, "socks-proxy-corp-1080");

        // The proxy alone: a profile without hops; its password to the keychain if asked.
        let plan = import_plan(socks, Engine::Mysql, Source::User, true, &name);
        assert_eq!(plan.connection.tunnel, name);
        let (tunnel, secrets) = plan.tunnel.unwrap();
        assert!(tunnel.hops.is_empty());
        let draft = tunnel.proxy.clone().unwrap();
        assert_eq!(
            (
                draft.kind,
                draft.host.as_str(),
                draft.port,
                draft.user.as_str()
            ),
            (ProxyType::Socks5, "proxy.corp", Some(1080), "proxy-user")
        );
        assert_eq!(draft.password, PasswordChoice::Keychain);
        assert_eq!(
            secrets.proxy.as_ref().map(Secret::expose),
            Some("proxy-secret")
        );
        let asked = import_plan(socks, Engine::Mysql, Source::User, false, &name);
        let (tunnel, secrets) = asked.tunnel.unwrap();
        assert_eq!(tunnel.proxy.unwrap().password, PasswordChoice::Prompt);
        assert!(secrets.proxy.is_none());

        // With SSH too, the proxy goes in front of the hop, in a profile of its own.
        let pg = found.iter().find(|c| c.id == "postgres-jdbc-1").unwrap();
        let mut both = pg.clone();
        both.proxy = socks.proxy.clone();
        assert_eq!(
            both.tunnel_name().unwrap(),
            "jump-bastion-example-com-via-proxy-corp-1080"
        );
        assert_eq!(pg.tunnel_name().unwrap(), "jump-bastion-example-com");
        let plan = import_plan(&both, Engine::Postgres, Source::User, true, "t");
        let (tunnel, secrets) = plan.tunnel.unwrap();
        assert_eq!(tunnel.hops.len(), 1);
        assert_eq!(tunnel.proxy.unwrap().host, "proxy.corp");
        assert_eq!(
            secrets.hops[0].as_ref().map(Secret::expose),
            Some("hop-pass")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn plans_keep_passwords_only_when_asked() {
        let root = workspace("plan");
        let found = read_dbeaver(&root).unwrap();
        let pg = found.iter().find(|c| c.id == "postgres-jdbc-1").unwrap();
        let kept = import_plan(
            pg,
            Engine::Postgres,
            Source::User,
            true,
            "bastion-example-com",
        );
        assert_eq!(kept.connection.password, PasswordChoice::Keychain);
        assert_eq!(kept.connection.tunnel, "bastion-example-com");
        assert_eq!(kept.connection.folder, "prod");
        assert_eq!(kept.password.as_ref().map(Secret::expose), Some("s3cret"));
        let (tunnel, secrets) = kept.tunnel.unwrap();
        assert_eq!(tunnel.hops[0].password, PasswordChoice::Keychain);
        assert_eq!(tunnel.options.keepalive_interval, Some(30));
        assert_eq!(tunnel.options.keepalive_count, Some(5));
        assert_eq!(tunnel.options.connect_timeout, Some(20));
        assert_eq!(kept.connection.local_port, Some(15432));
        assert_eq!(kept.connection.keepalive, Some(300));
        assert_eq!(
            secrets.hops[0].as_ref().map(Secret::expose),
            Some("hop-pass")
        );
        let asked = import_plan(pg, Engine::Postgres, Source::User, false, "t");
        assert_eq!(asked.connection.password, PasswordChoice::Prompt);
        assert!(asked.password.is_none());
        assert!(asked.tunnel.unwrap().1.hops[0].is_none());
        let sqlite = found.iter().find(|c| c.id == "sqlite_jdbc-3").unwrap();
        let plan = import_plan(sqlite, Engine::Sqlite, Source::User, true, "t");
        assert_eq!(plan.connection.password, PasswordChoice::None);
        assert!(plan.connection.host.is_empty() && plan.tunnel.is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
