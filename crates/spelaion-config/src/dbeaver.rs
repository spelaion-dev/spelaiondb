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

use crate::draft::SessionDraft;
use crate::import::{ImportedConnection, ImportedProxy, ImportedSsh, SavedSecrets};
use crate::linux_dirs::{LinuxHome, first_found};
use crate::model::{Engine, IsolationLevel, SshAuth, TunnelFile};

/// The key DBeaver ships with to obscure `credentials-config.json`.
const CREDENTIALS_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

#[derive(Debug, thiserror::Error)]
pub enum DbeaverError {
    #[error("no DBeaver configuration (.dbeaver/data-sources.json) under {0}")]
    NotFound(PathBuf),
    #[error("could not read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
}

/// Where DBeaver keeps its workspace on this system (it may not exist). On Linux, the first of
/// [`dbeaver_linux_workspaces`] that exists.
pub fn dbeaver_workspace() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|base| workspace_in(&PathBuf::from(base)))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| workspace_in(&PathBuf::from(home).join("Library")))
    } else {
        let home = LinuxHome::from_env()?;
        first_found(dbeaver_linux_workspaces(&home), |dir| dir.is_dir())
    }
}

/// DBeaver's workspace under the folder DBeaver keeps its data in.
fn workspace_in(base: &Path) -> PathBuf {
    base.join("DBeaverData").join("workspace6")
}

/// Where DBeaver's workspace may be on Linux, native install first.
fn dbeaver_linux_workspaces(home: &LinuxHome) -> Vec<PathBuf> {
    let mut dirs = vec![
        workspace_in(&home.data),
        // DBeaver reads XDG_DATA_HOME as a Java property, not from the environment, so without
        // one it uses `~/.local/share` (`RuntimeUtils.getWorkingDirectory`,
        // <https://github.com/dbeaver/dbeaver/blob/devel/plugins/org.jkiss.dbeaver.model/src/org/jkiss/dbeaver/utils/RuntimeUtils.java>).
        // Flatpak builds before 26.2.2 kept it there too.
        workspace_in(&home.home.join(".local/share")),
        // The Flatpak (io.dbeaver.DBeaverCommunity) passes its XDG_DATA_HOME on to DBeaver since
        // 26.2.2 (its `run.sh` and README: <https://github.com/flathub/io.dbeaver.DBeaverCommunity>).
        workspace_in(&home.flatpak("io.dbeaver.DBeaverCommunity").join("data")),
        // The snap (dbeaver-ce) is classic, but sets `-Duser.home=$SNAP_USER_DATA`
        // (<https://github.com/dbeaver/dbeaver/blob/devel/product/snap-build/snap/snapcraft.yaml>).
        workspace_in(&home.snap("dbeaver-ce").join(".local/share")),
    ];
    dirs.dedup();
    dirs
}

/// Every connection under `dir`: a DBeaver workspace (all its projects), one project, or a
/// `.dbeaver` folder.
pub fn read_dbeaver(dir: &Path) -> Result<Vec<ImportedConnection>, DbeaverError> {
    read_dbeaver_with(dir, SavedSecrets::Read)
}

/// [`read_dbeaver`], with or without the saved credentials (`credentials-config.json` is
/// neither opened nor decrypted with [`SavedSecrets::Skip`]).
pub fn read_dbeaver_with(
    dir: &Path,
    secrets: SavedSecrets,
) -> Result<Vec<ImportedConnection>, DbeaverError> {
    let folders = dbeaver_folders(dir);
    if folders.is_empty() {
        return Err(DbeaverError::NotFound(dir.to_path_buf()));
    }
    let mut out = Vec::new();
    for folder in folders {
        let credentials = match secrets {
            SavedSecrets::Read => read_credentials(&folder.join("credentials-config.json"))?,
            SavedSecrets::Skip => HashMap::new(),
        };
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
            let content = std::fs::read_to_string(&file).map_err(|e| unreadable(&file, e))?;
            let json: Json = serde_json::from_str(&content).map_err(|e| unreadable(&file, e))?;
            let Some(connections) = json.get("connections").and_then(Json::as_object) else {
                continue;
            };
            let types = json.get("connection-types");
            for (id, connection) in connections {
                let mut found = connection_of(id, connection, credentials.get(id));
                let dbeaver_type =
                    text(connection.get("configuration").and_then(|c| c.get("type")));
                found.env = closest_type(&dbeaver_type, types.and_then(|t| t.get(&dbeaver_type)));
                out.push(found);
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

/// The built-in type closest to a DBeaver connection type (ADR-029): DBeaver's own `dev`,
/// `test` and `prod` are ours; a custom one that asks before running or changing anything is
/// Production (never looser than it was), otherwise its id or name decides, and Development
/// when nothing does. DBeaver's colors are not imported.
pub(crate) fn closest_type(dbeaver: &str, definition: Option<&Json>) -> String {
    let id = crate::types::type_id(dbeaver);
    if id.is_empty() {
        return String::new();
    }
    if ["dev", "test", crate::types::PRODUCTION].contains(&id.as_str()) {
        return id;
    }
    let flag =
        |key: &str| definition.and_then(|d| d.get(key)).and_then(Json::as_bool) == Some(true);
    if flag("confirm-data-change") || flag("confirm-execute") {
        return crate::types::PRODUCTION.into();
    }
    let words = format!(
        "{id} {}",
        text(definition.and_then(|d| d.get("name"))).to_lowercase()
    );
    let has = |parts: &[&str]| parts.iter().any(|p| words.contains(p));
    if has(&["prod", "live"]) {
        crate::types::PRODUCTION.into()
    } else if has(&["test", "qa", "uat", "stag", "preprod"]) {
        "test".into()
    } else {
        "dev".into()
    }
}

fn connection_of(id: &str, json: &Json, credentials: Option<&Json>) -> ImportedConnection {
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
    // These driver IDs are declared by DBeaver's PostgreSQL provider.
    let driver_kind = match driver.as_str() {
        "postgres-cockroach-jdbc" => "cockroachdb",
        "postgres-yugabytedb-jdbc" => "yugabytedb",
        "postgres-timescale-jdbc" => "timescaledb",
        id => id,
    };
    let engine_kind = [driver_kind, provider.as_str()]
        .into_iter()
        .find(|kind| crate::engine_kind_base(kind).is_some_and(|base| engine.as_ref() == Ok(&base)))
        .map(str::to_owned);
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
    let mut unmapped = unmapped_of(json, config);
    let session = match &engine {
        Ok(engine) => session_of(config, *engine, &mut unmapped),
        Err(_) => SessionDraft::default(),
    };
    ImportedConnection {
        id: id.to_string(),
        name: text(json.get("name")),
        description: text(json.get("description")),
        tags: json
            .get("tags")
            .and_then(Json::as_object)
            .map(|map| {
                map.iter()
                    .map(|(name, value)| match text(Some(value)) {
                        value if value.is_empty() || value == name.trim() => {
                            name.trim().to_string()
                        }
                        value => format!("{}={value}", name.trim()),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        folder: text(json.get("folder")),
        engine,
        engine_kind,
        host: text(config.get("host")),
        port: port(config.get("port")),
        database,
        user,
        password: password.into(),
        env: closest_type(&text(config.get("type")), None),
        read_only: config.get("read-only").and_then(Json::as_bool) == Some(true),
        ssh: ssh_of(config, credentials),
        proxy: proxy_of(config, credentials),
        tls: None,
        session,
        unmapped,
    }
}

/// DBeaver's per-connection bootstrap (SDB-99) as the connection's session setup (ADR-037):
/// its queries, one statement each, run on every new session ("ignore errors" as DBeaver's),
/// its default schema (or, where a catalog is the database a session switches to, MySQL's
/// and MariaDB's, its default catalog) and its isolation level where the engine has that one.
/// The setup is checked by the core before it is shown or saved; what does not map is listed.
fn session_of(config: &Json, engine: Engine, unmapped: &mut Vec<String>) -> SessionDraft {
    let Some(bootstrap) = config.get("bootstrap") else {
        return SessionDraft::default();
    };
    let queries: Vec<String> = bootstrap
        .get("initQueries")
        .and_then(Json::as_array)
        .map(|queries| {
            queries
                .iter()
                .filter_map(Json::as_str)
                .map(|q| q.trim().trim_end_matches(';').trim().to_string())
                .filter(|q| !q.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let setup = if queries.is_empty() {
        String::new()
    } else {
        format!("{};", queries.join(";\n"))
    };
    let catalog = text(bootstrap.get("defaultCatalog"));
    let mut schema = text(bootstrap.get("defaultSchema"));
    if !catalog.is_empty() {
        match engine {
            // A MySQL or MariaDB "catalog" is a database, which is what `USE` switches to.
            Engine::Mysql | Engine::Mariadb if schema.is_empty() => schema = catalog,
            _ => unmapped.push(format!("Default catalog: {catalog}")),
        }
    }
    // A JDBC level (`java.sql.Connection`'s constants; SQL Server's own for snapshot).
    let level = bootstrap
        .get("txnIsolation")
        .or_else(|| bootstrap.get("defaultTransactionIsolation"))
        .and_then(Json::as_i64);
    let isolation = level.and_then(|level| {
        let mapped = match level {
            1 => Some(IsolationLevel::ReadUncommitted),
            2 => Some(IsolationLevel::ReadCommitted),
            4 => Some(IsolationLevel::RepeatableRead),
            8 => Some(IsolationLevel::Serializable),
            4096 => Some(IsolationLevel::Snapshot),
            _ => None,
        };
        match mapped {
            Some(isolation) if isolation.available_on(engine) => Some(isolation),
            Some(isolation) => {
                unmapped.push(format!(
                    "Isolation level: {} ({} has no such session level)",
                    isolation.sql(),
                    engine.label()
                ));
                None
            }
            None => {
                unmapped.push(format!("Isolation level: {level}"));
                None
            }
        }
    });
    if bootstrap.get("autocommit").and_then(Json::as_bool) == Some(false) {
        unmapped
            .push("Auto-commit: off (a connection type with manual commit does that here)".into());
    }
    SessionDraft {
        ignore_setup_errors: !setup.is_empty()
            && bootstrap.get("ignoreErrors").and_then(Json::as_bool) == Some(true),
        setup,
        schema,
        isolation,
    }
}

/// What a DBeaver connection sets that the import leaves out, for the preview: other network
/// handlers (its SSL settings, HTTP tunnels…), driver properties and other ways to log in. A
/// property that may hold a secret shows its name only. Its bootstrap is the session setup
/// (`session_of`).
fn unmapped_of(json: &Json, config: &Json) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(handlers) = config.get("handlers").and_then(Json::as_object) {
        for (id, handler) in handlers {
            let enabled = handler.get("enabled").and_then(Json::as_bool) == Some(true);
            if enabled && !matches!(id.as_str(), "ssh_tunnel" | "socks_proxy") {
                out.push(format!("Network handler: {id}"));
            }
        }
    }
    let auth = text(config.get("auth-model"));
    if !auth.is_empty() && !auth.starts_with("native") {
        out.push(format!("Authentication: {auth}"));
    }
    for key in ["provider-properties", "properties"] {
        if let Some(properties) = config.get(key).and_then(Json::as_object) {
            for (name, value) in properties {
                let lower = name.to_lowercase();
                if !lower.starts_with("@dbeaver") {
                    out.push(format!("Driver property: {name}"));
                } else if lower == "@dbeaver-show-non-default-db@"
                    && (value.as_bool() == Some(true) || text(Some(value)) == "true")
                {
                    // ADR-074, point 12: never turned into `all_databases` by an import.
                    out.push(
                        "Show all databases (turn on all databases in the connection form)".into(),
                    );
                }
            }
        }
    }
    if json.get("lockPasswordHash").is_some() {
        out.push("A DBeaver lock password".into());
    }
    out
}

fn proxy_of(config: &Json, credentials: Option<&Json>) -> Option<ImportedProxy> {
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
    Some(ImportedProxy {
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

fn ssh_of(config: &Json, credentials: Option<&Json>) -> Option<ImportedSsh> {
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
    Some(ImportedSsh {
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

#[cfg(test)]
mod tests {
    #[test]
    fn an_explicit_dbeaver_driver_gives_the_kind_but_a_port_does_not() {
        let source = serde_json::json!({
            "provider": "postgresql",
            "driver": "postgres-cockroach-jdbc",
            "configuration": { "host": "localhost", "port": "26257" },
        });
        let connection = super::connection_of("one", &source, None);
        assert_eq!(connection.engine, Ok(super::Engine::Postgres));
        assert_eq!(connection.engine_kind.as_deref(), Some("cockroachdb"));
        let mut ordinary = source;
        ordinary["driver"] = "postgres-jdbc".into();
        assert_eq!(
            super::connection_of("two", &ordinary, None).engine_kind,
            None
        );
    }

    use super::*;
    use crate::draft::PasswordChoice;
    use crate::import::{TargetFolder, import_plan};
    use crate::load::Config;
    use crate::load::Source;
    use crate::model::ProxyType;
    use cbc::Encryptor;
    use cbc::cipher::BlockEncryptMut;

    /// DBeaver's folders, kept as they are.
    fn keep() -> TargetFolder {
        TargetFolder {
            folder: String::new(),
            keep_client_folders: true,
        }
    }

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
              "connection-types": {
                "prod": {"name": "Production", "color": "247,159,129", "confirm-execute": true},
                "QA Team": {"name": "QA team", "color": "0,128,255", "description": "Shared QA",
                  "auto-commit": false, "confirm-execute": true, "confirm-data-change": true,
                  "smart-commit": true, "auto-close-transactions": true,
                  "auto-close-connections": true, "close-connections-period": 900},
                "white": {"name": "Plain", "color": "255,255,255"}
              },
              "connections": {
                "postgres-jdbc-1": {
                  "provider": "postgresql", "driver": "postgres-jdbc", "name": "Orders PG",
                  "description": "Orders of the EU shop.\nAsk #data before heavy queries.",
                  "tags": {"team": "", "region": "eu", "billing": "billing"},
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
                  "configuration": {"host": "localhost", "port": "3306", "database": "shop", "type": "QA Team"}
                },
                "sqlite_jdbc-3": {
                  "provider": "sqlite", "driver": "sqlite_jdbc", "name": "Local file",
                  "configuration": {"database": "C:\\data\\local.db", "type": "dev"}
                },
                "oracle_thin-4": {
                  "provider": "oracle", "driver": "oracle_thin", "name": "Legacy",
                  "configuration": {"host": "ora", "port": "1521", "type": "white"}
                },
                "duckdb_jdbc-5": {
                  "provider": "duckdb", "driver": "duckdb_jdbc", "name": "Analytics",
                  "configuration": {"url": "jdbc:duckdb:C:\\data\\sales.duckdb", "type": "dev"}
                },
                "mysql8-6": {
                  "provider": "mysql", "driver": "mysql8", "name": "Behind SOCKS", "save-password": true,
                  "configuration": {
                    "host": "localhost", "port": "3306", "type": "dev",
                    "provider-properties": {"useCompression": "true", "@dbeaver-default-dbname": "x"},
                    "handlers": {"socks_proxy": {"type": "PROXY", "enabled": true, "save-password": true,
                      "properties": {"socks-host": "proxy.corp", "socks-port": 1080}},
                      "mysql_ssl": {"type": "CONFIG", "enabled": true, "properties": {}},
                      "off": {"type": "CONFIG", "enabled": false}}
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

    /// Moves the workspace `workspace` wrote to `to`, where an install keeps it.
    fn move_workspace(name: &str, to: &Path) {
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::rename(workspace(name), to).unwrap();
    }

    fn found_on_linux(home: &Path) -> PathBuf {
        let home = LinuxHome::at(home);
        first_found(dbeaver_linux_workspaces(&home), |dir| dir.is_dir()).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        read_dbeaver(dir)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn linux_search_finds_the_flatpak_workspace() {
        let home = crate::linux_dirs::testing::fake_home("dbeaver-flatpak");
        let flatpak = home.join(".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6");
        move_workspace("linux-flatpak", &flatpak);
        assert_eq!(found_on_linux(&home), flatpak);
        assert!(names(&flatpak).contains(&"Orders PG".to_string()));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn linux_search_finds_the_snap_workspace() {
        let home = crate::linux_dirs::testing::fake_home("dbeaver-snap");
        let revision = crate::linux_dirs::testing::snap_revision(&home, "dbeaver-ce", "412");
        move_workspace(
            "linux-snap",
            &revision.join(".local/share/DBeaverData/workspace6"),
        );
        let found = found_on_linux(&home);
        assert_eq!(
            found,
            home.join("snap/dbeaver-ce/current/.local/share/DBeaverData/workspace6")
        );
        assert!(names(&found).contains(&"Orders PG".to_string()));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn linux_search_prefers_the_native_workspace_and_names_it_when_none_exists() {
        let home = crate::linux_dirs::testing::fake_home("dbeaver-native");
        let native = home.join(".local/share/DBeaverData/workspace6");
        assert_eq!(found_on_linux(&home), native);
        move_workspace(
            "linux-native-flatpak",
            &home.join(".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6"),
        );
        move_workspace("linux-native", &native);
        assert_eq!(found_on_linux(&home), native);
        let _ = std::fs::remove_dir_all(&home);
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
        assert_eq!(pg.password.secret().map(Secret::expose), Some("s3cret"));
        assert_eq!(
            pg.description,
            "Orders of the EU shop.\nAsk #data before heavy queries."
        );
        assert_eq!(by_id("mariaDB-2").description, "");
        // A tag without a value (or with its own name as value) is its name.
        let mut tags = pg.tags.clone();
        tags.sort();
        assert_eq!(tags, ["billing", "region=eu", "team"]);
        assert!(by_id("mariaDB-2").tags.is_empty());
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
        // DBeaver's types map to the closest built-in one; its colors are ignored.
        assert_eq!(pg.env, "prod");
        assert_eq!(
            maria.env, "prod",
            "a custom type that confirms changes stays as strict"
        );
        assert_eq!(by_id("oracle_thin-4").env, "dev");
        assert_eq!(by_id("sqlite_jdbc-3").env, "dev");
        assert_eq!(
            import_plan(maria, Engine::Mariadb, Source::User, false, "", &keep())
                .connection
                .env,
            "prod"
        );
        let quiet = |name: &str| serde_json::json!({ "name": name });
        assert_eq!(closest_type("QA", Some(&quiet("QA team"))), "test");
        assert_eq!(closest_type("x1", Some(&quiet("Staging"))), "test");
        assert_eq!(closest_type("x2", Some(&quiet("Live shop"))), "prod");
        assert_eq!(closest_type("Production", None), "prod");
        assert_eq!(closest_type("sandbox", Some(&quiet("Sandbox"))), "dev");
        assert_eq!(closest_type("", None), "");
        assert_eq!(by_id("sqlite_jdbc-3").database, "C:\\data\\local.db");
        assert!(by_id("oracle_thin-4").engine.is_err());
        // What the import leaves out is listed, never dropped silently.
        assert_eq!(
            by_id("mysql8-6").unmapped,
            [
                "Network handler: mysql_ssl",
                "Driver property: useCompression"
            ]
        );
        assert!(pg.unmapped.is_empty());
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
        let plan = import_plan(socks, Engine::Mysql, Source::User, true, &name, &keep());
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
        let asked = import_plan(socks, Engine::Mysql, Source::User, false, &name, &keep());
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
        let plan = import_plan(&both, Engine::Postgres, Source::User, true, "t", &keep());
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
            &keep(),
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
            kept.connection.description,
            "Orders of the EU shop.\nAsk #data before heavy queries."
        );
        let mut tags = kept.connection.tags.clone();
        tags.sort();
        assert_eq!(tags, ["billing", "region=eu", "team"]);
        assert_eq!(
            secrets.hops[0].as_ref().map(Secret::expose),
            Some("hop-pass")
        );
        let asked = import_plan(pg, Engine::Postgres, Source::User, false, "t", &keep());
        assert_eq!(asked.connection.password, PasswordChoice::Prompt);
        assert!(asked.password.is_none());
        assert!(asked.tunnel.unwrap().1.hops[0].is_none());
        let sqlite = found.iter().find(|c| c.id == "sqlite_jdbc-3").unwrap();
        let plan = import_plan(sqlite, Engine::Sqlite, Source::User, true, "t", &keep());
        assert_eq!(plan.connection.password, PasswordChoice::None);
        assert!(plan.connection.host.is_empty() && plan.tunnel.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    /// A DBeaver connection of `provider`/`driver` with this `bootstrap`.
    fn bootstrapped(provider: &str, driver: &str, bootstrap: Json) -> ImportedConnection {
        let source = serde_json::json!({
            "provider": provider, "driver": driver, "name": "Orders",
            "configuration": {"host": "db.internal", "port": "5432", "database": "orders",
              "user": "app", "bootstrap": bootstrap}
        });
        connection_of("one", &source, None)
    }

    #[test]
    fn bootstrap_queries_schema_and_isolation_are_the_session_setup() {
        let orders = bootstrapped(
            "postgresql",
            "postgres-jdbc",
            serde_json::json!({
                "initQueries": ["SET search_path TO app, public", "  set role reader;  ", ""],
                "ignoreErrors": true,
                "defaultCatalog": "orders",
                "defaultSchema": "app",
                "txnIsolation": 2,
                "autocommit": false
            }),
        );
        assert_eq!(
            orders.session,
            SessionDraft {
                setup: "SET search_path TO app, public;\nset role reader;".into(),
                ignore_setup_errors: true,
                schema: "app".into(),
                isolation: Some(IsolationLevel::ReadCommitted),
            }
        );
        // A PostgreSQL catalog is another database, and auto-commit is a type's matter here.
        assert!(
            orders
                .unmapped
                .contains(&"Default catalog: orders".to_string())
        );
        assert!(
            orders
                .unmapped
                .iter()
                .any(|u| u.starts_with("Auto-commit: off"))
        );
        assert!(!orders.unmapped.iter().any(|u| u.contains("Bootstrap")));
        // The draft and the file it becomes carry the same values (the core checks the setup
        // first, `spelaion_core::vet_imported_setup`).
        let plan = import_plan(&orders, Engine::Postgres, Source::User, false, "t", &keep());
        assert_eq!(plan.connection.session, orders.session);
        let file = plan.connection.to_file(&Config::default()).unwrap();
        assert_eq!(
            file.session.setup.as_deref(),
            Some("SET search_path TO app, public;\nset role reader;")
        );
        assert!(file.session.ignore_setup_errors);
        assert_eq!(file.session.schema.as_deref(), Some("app"));
        assert_eq!(file.session.isolation, Some(IsolationLevel::ReadCommitted));
    }

    #[test]
    fn a_catalog_is_the_schema_where_it_is_a_database_and_levels_must_exist() {
        // MySQL's catalog is the database `USE` switches to; snapshot is SQL Server's alone.
        let shop = bootstrapped(
            "mysql",
            "mysql8",
            serde_json::json!({"defaultCatalog": "shop", "txnIsolation": 4096}),
        );
        assert_eq!(shop.session.schema, "shop");
        assert_eq!(shop.session.isolation, None);
        assert!(
            shop.unmapped
                .contains(&"Isolation level: SNAPSHOT (MySQL has no such session level)".into())
        );
        let reports = bootstrapped(
            "sqlserver",
            "mssql_jdbc_ms_new",
            serde_json::json!({"defaultCatalog": "reports", "defaultSchema": "dbo",
              "txnIsolation": 4096}),
        );
        assert_eq!(reports.session.schema, "dbo");
        assert_eq!(reports.session.isolation, Some(IsolationLevel::Snapshot));
        assert!(
            reports
                .unmapped
                .contains(&"Default catalog: reports".into())
        );
        // An unknown level is listed; "ignore errors" means nothing without queries.
        let other = bootstrapped(
            "postgresql",
            "postgres-jdbc",
            serde_json::json!({"txnIsolation": 3, "ignoreErrors": true}),
        );
        assert_eq!(other.session, SessionDraft::default());
        assert!(other.unmapped.contains(&"Isolation level: 3".into()));
        // No bootstrap: no session setup, nothing listed for it.
        let plain = bootstrapped("postgresql", "postgres-jdbc", Json::Null);
        assert_eq!(plain.session, SessionDraft::default());
        assert!(plain.unmapped.is_empty(), "{:?}", plain.unmapped);
    }

    /// DBeaver's "Show all databases" is listed as not imported, never turned into
    /// `all_databases` (ADR-074, point 12): the user turns that on in the form.
    #[test]
    fn show_all_databases_is_listed_and_never_turned_on() {
        let none = serde_json::json!({});
        let on = serde_json::json!({"provider-properties": {
            "@dbeaver-show-non-default-db@": "true", "@dbeaver-default-dbname": "x"}});
        assert_eq!(
            unmapped_of(&none, &on),
            ["Show all databases (turn on all databases in the connection form)"]
        );
        let off =
            serde_json::json!({"provider-properties": {"@dbeaver-show-non-default-db@": "false"}});
        assert!(unmapped_of(&none, &off).is_empty());
    }
}
