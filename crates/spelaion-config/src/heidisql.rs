//! HeidiSQL's exported settings and, on Windows, its saved registry sessions.
//!
//! The text export uses `path<|||>type<|||>value`, with type 1 for strings and 3 for
//! integers. The classic Windows client stores the same values under
//! `HKCU\Software\HeidiSQL\Servers`. Portable exports and the registry share one reader.
//! The format is untrusted: inputs are bounded, malformed settings fail closed, and no
//! password or other sensitive value is ever included in an error or preview.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use spelaion_driver_api::Secret;

use crate::import::{ImportedConnection, ImportedSsh, ImportedTls};
use crate::model::{Engine, SshAuth, TlsModeConfig};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FIELDS: usize = 100_000;
const MAX_DEPTH: usize = 32;
const DELIMITER: &str = "<|||>";
type Fields = BTreeMap<String, String>;
type Sessions = BTreeMap<String, Fields>;

#[derive(Debug, thiserror::Error)]
pub enum HeidiSqlError {
    #[error("could not read HeidiSQL settings from {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
    #[error("invalid HeidiSQL settings: {0}")]
    Invalid(&'static str),
    #[error(
        "HeidiSQL registry settings are available only on Windows; choose an exported settings file"
    )]
    RegistryUnavailable,
    #[error("could not read HeidiSQL's Windows registry settings: {0}")]
    Registry(String),
}

fn insert(
    sessions: &mut Sessions,
    path: &str,
    name: &str,
    value: String,
) -> Result<(), HeidiSqlError> {
    let path = path.trim_matches('\\');
    let parts: Vec<_> = path.split('\\').collect();
    if path.is_empty()
        || parts.len() > MAX_DEPTH
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
        || name.is_empty()
    {
        return Err(HeidiSqlError::Invalid("invalid session path"));
    }
    let fields = sessions.entry(path.to_string()).or_default();
    if fields.insert(name.to_ascii_lowercase(), value).is_some() {
        return Err(HeidiSqlError::Invalid("duplicate session setting"));
    }
    Ok(())
}

/// Parse an export, not a Windows `.reg` file. HeidiSQL's line endings may be CRLF or LF.
fn parse_export(content: &str) -> Result<Sessions, HeidiSqlError> {
    let mut sessions = Sessions::new();
    let mut count = 0usize;
    for line in content.trim_start_matches('\u{feff}').lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((full, rest)) = line.split_once(DELIMITER) else {
            return Err(HeidiSqlError::Invalid("a line has no delimiter"));
        };
        let Some((kind, value)) = rest.split_once(DELIMITER) else {
            return Err(HeidiSqlError::Invalid("a line has no value"));
        };
        // Global settings and query history are not connections and may contain user SQL.
        let Some((root, setting)) = full.split_once('\\') else {
            continue;
        };
        if !root.eq_ignore_ascii_case("Servers") {
            continue;
        }
        count += 1;
        if count > MAX_FIELDS {
            return Err(HeidiSqlError::Invalid("too many settings"));
        }
        if !matches!(kind, "1" | "3") {
            return Err(HeidiSqlError::Invalid("unsupported setting type"));
        }
        let Some((path, name)) = setting.rsplit_once('\\') else {
            continue;
        };
        let value = value.replace("<{{{>", "\r").replace("<}}}>", "\n");
        insert(&mut sessions, path, name, value)?;
    }
    Ok(sessions)
}

/// Read an exported settings file. Passwords remain `Secret`s, never preview text.
pub fn read_heidisql(path: &Path) -> Result<Vec<ImportedConnection>, HeidiSqlError> {
    let file = std::fs::File::open(path).map_err(|e| HeidiSqlError::Unreadable {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| HeidiSqlError::Unreadable {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(HeidiSqlError::Invalid("settings file exceeds 8 MiB"));
    }
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| HeidiSqlError::Invalid("settings file is not UTF-8"))?;
    Ok(convert(parse_export(content)?))
}

/// Whether this system has HeidiSQL's registry sessions to read: only Windows does. Elsewhere
/// HeidiSQL's connections can only come from a settings file the user exported from it, so the
/// importer starts by asking for one and never offers the registry.
pub fn heidisql_has_registry() -> bool {
    cfg!(windows)
}

/// Read the classic HeidiSQL sessions in the current user's registry, only on explicit import.
#[cfg(windows)]
pub fn read_heidisql_registry() -> Result<Vec<ImportedConnection>, HeidiSqlError> {
    let root = windows_registry::CURRENT_USER
        .open(r"Software\HeidiSQL\Servers")
        .map_err(|e| HeidiSqlError::Registry(e.to_string()))?;
    Ok(convert(registry_sessions(&root)?))
}

#[cfg(not(windows))]
pub fn read_heidisql_registry() -> Result<Vec<ImportedConnection>, HeidiSqlError> {
    Err(HeidiSqlError::RegistryUnavailable)
}

#[cfg(windows)]
fn registry_sessions(root: &windows_registry::Key) -> Result<Sessions, HeidiSqlError> {
    registry_sessions_bounded(root, MAX_FIELDS, MAX_BYTES)
}

#[cfg(windows)]
fn registry_sessions_bounded(
    root: &windows_registry::Key,
    max_fields: usize,
    max_bytes: u64,
) -> Result<Sessions, HeidiSqlError> {
    fn walk(
        key: &windows_registry::Key,
        path: &str,
        sessions: &mut Sessions,
        count: &mut usize,
        bytes: &mut usize,
        max_fields: usize,
        max_bytes: u64,
    ) -> Result<(), HeidiSqlError> {
        if path.split('\\').count() > MAX_DEPTH {
            return Err(HeidiSqlError::Invalid("registry nesting is too deep"));
        }
        for (name, value) in key
            .values()
            .map_err(|e| HeidiSqlError::Registry(e.to_string()))?
        {
            *count += 1;
            *bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
            if *count > max_fields || *bytes as u64 > max_bytes {
                return Err(HeidiSqlError::Invalid("registry settings exceed the limit"));
            }
            let text = match value.ty() {
                windows_registry::Type::String => {
                    String::try_from(value).map_err(|e| HeidiSqlError::Registry(e.to_string()))?
                }
                windows_registry::Type::U32 => u32::try_from(value)
                    .map_err(|e| HeidiSqlError::Registry(e.to_string()))?
                    .to_string(),
                _ => continue,
            };
            if !path.is_empty() {
                insert(sessions, path, &name, text)?;
            }
        }
        for name in key
            .keys()
            .map_err(|e| HeidiSqlError::Registry(e.to_string()))?
        {
            *count += 1;
            *bytes = bytes.saturating_add(name.len());
            if *count > max_fields || *bytes as u64 > max_bytes {
                return Err(HeidiSqlError::Invalid("registry settings exceed the limit"));
            }
            let child = key
                .open(&name)
                .map_err(|e| HeidiSqlError::Registry(e.to_string()))?;
            let child_path = if path.is_empty() {
                name
            } else {
                format!("{path}\\{name}")
            };
            walk(
                &child,
                &child_path,
                sessions,
                count,
                bytes,
                max_fields,
                max_bytes,
            )?;
        }
        Ok(())
    }
    let mut sessions = Sessions::new();
    walk(
        root,
        "",
        &mut sessions,
        &mut 0,
        &mut 0,
        max_fields,
        max_bytes,
    )?;
    Ok(sessions)
}

/// HeidiSQL's reversible obfuscation, matching `encrypt`/`encryptUnicode` in apphelpers.pas.
/// Invalid data is never included in an error. A salt alone encodes an empty password.
fn password(encoded: &str) -> Result<Option<Secret>, ()> {
    if encoded.is_empty() {
        return Ok(None);
    }
    if !encoded.is_ascii() {
        return Err(());
    }
    let (body, flag) = encoded.split_at(encoded.len() - 1);
    let (body, salt, width) = if flag == "0" {
        if body.is_empty() {
            return Err(());
        }
        let (value, salt) = body.split_at(body.len() - 1);
        (value, salt.parse::<u16>().map_err(|_| ())?, 4)
    } else {
        (body, flag.parse::<u16>().map_err(|_| ())?, 2)
    };
    if salt == 0 || salt > 9 || !body.is_ascii() || body.len() % width != 0 {
        return Err(());
    }
    let mut chars = Vec::new();
    for chunk in body.as_bytes().chunks(width) {
        let hex = std::str::from_utf8(chunk).map_err(|_| ())?;
        let value = u16::from_str_radix(hex, 16).map_err(|_| ())?;
        let decoded = if width == 4 {
            value.wrapping_sub(salt)
        } else if value < salt {
            value + 255 - salt
        } else {
            value - salt
        };
        if decoded == 0 {
            return Err(());
        }
        chars.push(decoded);
    }
    let text = String::from_utf16(&chars).map_err(|_| ())?;
    Ok((!text.is_empty()).then(|| Secret::new(text)))
}

fn field<'a>(fields: &'a Fields, name: &str) -> &'a str {
    fields.get(name).map(String::as_str).unwrap_or_default()
}

fn flag(fields: &Fields, name: &str) -> bool {
    field(fields, name) == "1"
}

fn port(fields: &Fields, name: &str, warnings: &mut Vec<String>) -> Option<u16> {
    let value = field(fields, name);
    if value.is_empty() {
        return None;
    }
    match value.parse::<u16>() {
        Ok(p) if p > 0 => Some(p),
        _ => {
            warnings.push(format!("{name}: invalid port"));
            None
        }
    }
}

fn convert(sessions: Sessions) -> Vec<ImportedConnection> {
    sessions
        .into_iter()
        .filter_map(|(path, fields)| {
            if fields.contains_key("folder")
                || (!fields.contains_key("nettype") && !fields.contains_key("host"))
            {
                return None;
            }
            Some(connection(path, fields))
        })
        .collect()
}

fn connection(path: String, fields: Fields) -> ImportedConnection {
    let mut warnings = Vec::new();
    let (folder, name) = path
        .rsplit_once('\\')
        .map_or(("", path.as_str()), |(folder, name)| (folder, name));
    let net = if field(&fields, "nettype").is_empty() {
        Some(0)
    } else {
        field(&fields, "nettype").parse::<u8>().ok()
    };
    let server_version = field(&fields, "serverversionfull").to_ascii_lowercase();
    let mut engine = match net {
        Some(0 | 2 | 16) => Ok(if server_version.contains("mariadb") {
            Engine::Mariadb
        } else {
            Engine::Mysql
        }),
        Some(4) => Ok(Engine::Sqlserver),
        Some(8 | 9) => Ok(Engine::Postgres),
        Some(10) => Ok(Engine::Sqlite),
        Some(1) => Err("MySQL named pipes are not supported".into()),
        Some(3 | 5..=7) => Err("this SQL Server transport is not supported".into()),
        Some(17) => Err("encrypted SQLite databases are not supported".into()),
        _ => Err("this HeidiSQL connection type is not supported".into()),
    };
    if flag(&fields, "windowsauth") {
        engine = Err("Windows authentication is not supported for imported connections".into());
    }
    let raw_host = field(&fields, "host");
    let multiple_files =
        engine.as_ref().is_ok_and(|e| *e == Engine::Sqlite) && raw_host.contains('|');
    if multiple_files {
        warnings.push("SQLite: only the first database file is imported".into());
    }
    let file = raw_host.split('|').next().unwrap_or_default().to_string();
    let databases = field(&fields, "databases");
    let database = match engine {
        Ok(Engine::Sqlite) => file,
        Ok(Engine::Postgres | Engine::Sqlserver) if !databases.contains(';') => {
            databases.to_string()
        }
        _ => {
            if !databases.is_empty() {
                warnings.push("Databases: HeidiSQL's database filter is not imported".into());
            }
            String::new()
        }
    };
    let ssh_on = fields
        .get("sshtunnelactive")
        .map_or(matches!(net, Some(2 | 9 | 16)), |v| v == "1");
    // HeidiSQL does not start a tunnel for MySQL TCP or SQLite even if an inconsistent
    // session carries this flag. Do not silently change the route of that connection.
    if ssh_on && matches!(net, Some(0 | 10)) {
        engine = Err("SSH is active for a HeidiSQL transport that does not use a tunnel".into());
    }
    let key = field(&fields, "sshtunnelprivatekey").to_string();
    let (ssh_password, invalid_ssh_password) = match password(field(&fields, "sshtunnelpassword")) {
        Ok(value) => (value, false),
        Err(()) => {
            warnings.push(if !ssh_on {
                "SSH password: not imported because SSH is disabled".into()
            } else if key.is_empty() {
                "SSH password: could not be decoded; it will be requested".into()
            } else {
                "SSH password: could not be decoded and is not imported with a private key".into()
            });
            (None, true)
        }
    };
    if !ssh_on && ssh_password.is_some() {
        warnings.push("SSH password: not imported because SSH is disabled".into());
    }
    let ssh = if ssh_on {
        let host = field(&fields, "sshtunnelhost");
        let host = if host.is_empty() { raw_host } else { host };
        let auth = if !key.is_empty() {
            SshAuth::Key
        } else if ssh_password.is_some() || invalid_ssh_password {
            SshAuth::Password
        } else {
            SshAuth::Agent
        };
        if key.to_ascii_lowercase().ends_with(".ppk") {
            warnings.push("SSH key: a PuTTY key may need converting to OpenSSH format".into());
        }
        if !key.is_empty() && ssh_password.is_some() {
            warnings.push("SSH password: not imported with a private key".into());
        }
        let timeout = field(&fields, "sshtunneltimeout")
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0);
        Some(ImportedSsh {
            host: host.to_string(),
            port: port(&fields, "sshtunnelhostport", &mut warnings),
            user: field(&fields, "sshtunneluser").to_string(),
            auth,
            key_path: key,
            password: if auth == SshAuth::Password {
                ssh_password
            } else {
                None
            },
            keepalive_interval: None,
            keepalive_count: None,
            connect_timeout: timeout,
            // HeidiSQL stores 0 for an automatically chosen local forwarding port.
            local_port: if field(&fields, "sshtunnelport") == "0" {
                None
            } else {
                port(&fields, "sshtunnelport", &mut warnings)
            },
        })
    } else {
        None
    };
    let tls = if matches!(
        engine,
        Ok(Engine::Postgres | Engine::Mysql | Engine::Mariadb)
    ) {
        // Old HeidiSQL sessions turn SSL on implicitly when a client private key is set.
        let ssl_on = fields
            .get("ssl_active")
            .map_or(!field(&fields, "ssl_key").is_empty(), |v| v == "1");
        let mode = if ssl_on {
            match field(&fields, "ssl_verification") {
                "0" => {
                    if matches!(engine, Ok(Engine::Mysql | Engine::Mariadb)) {
                        warnings
                            .push("TLS: this MySQL connection may fall back to plaintext".into());
                        TlsModeConfig::Prefer
                    } else {
                        TlsModeConfig::Require
                    }
                }
                "1" => TlsModeConfig::VerifyCa,
                "" | "2" => TlsModeConfig::VerifyFull,
                _ => {
                    engine = Err("unknown TLS verification mode".into());
                    TlsModeConfig::VerifyFull
                }
            }
        } else {
            TlsModeConfig::Disable
        };
        Some(ImportedTls {
            mode,
            ca: field(&fields, "ssl_ca").to_string(),
            cert: field(&fields, "ssl_cert").to_string(),
            key: field(&fields, "ssl_key").to_string(),
        })
    } else {
        None
    };
    if matches!(engine, Ok(Engine::Sqlserver | Engine::Sqlite))
        && (flag(&fields, "ssl_active")
            || ["ssl_ca", "ssl_cert", "ssl_key"]
                .iter()
                .any(|name| !field(&fields, name).is_empty())
            || matches!(field(&fields, "ssl_verification"), "0" | "1"))
    {
        warnings.push("TLS: these settings are not imported for this engine".into());
    }
    let pass = password(field(&fields, "password")).unwrap_or_else(|_| {
        warnings.push("Password: could not be decoded; it will be requested".into());
        None
    });
    for name in ["windowsauth", "sshtunnelactive", "ssl_active"] {
        if let Some(value) = fields.get(name)
            && !matches!(value.as_str(), "0" | "1")
        {
            engine = Err(format!("invalid {name} setting"));
        }
    }
    const HANDLED: &[&str] = &[
        "folder",
        "host",
        "user",
        "password",
        "port",
        "nettype",
        "databases",
        "comment",
        "serverversion",
        "serverversionfull",
        "connectcount",
        "lastconnect",
        "sessioncreated",
        "sshtunnelactive",
        "sshtunnelhost",
        "sshtunnelhostport",
        "sshtunneluser",
        "sshtunnelpassword",
        "sshtunneltimeout",
        "sshtunnelprivatekey",
        "sshtunnelport",
        "ssl_active",
        "ssl_key",
        "ssl_cert",
        "ssl_ca",
        "ssl_verification",
        "windowsauth",
    ];
    for (key, value) in &fields {
        if !HANDLED.contains(&key.as_str()) && !value.is_empty() && value != "0" {
            warnings.push(format!("HeidiSQL setting: {key} (value not shown)"));
        }
    }
    // Imported files and registry values are untrusted, and a variable reference must not
    // turn into a reference to this computer's environment or command-based secret.
    if [
        raw_host,
        field(&fields, "user"),
        databases,
        field(&fields, "sshtunnelhost"),
        field(&fields, "sshtunneluser"),
        field(&fields, "sshtunnelprivatekey"),
        field(&fields, "ssl_ca"),
        field(&fields, "ssl_cert"),
        field(&fields, "ssl_key"),
    ]
    .iter()
    .any(|s| s.contains("${"))
    {
        engine =
            Err("variable references in imported settings must be removed before import".into());
    }
    let database_port = port(&fields, "port", &mut warnings);
    if warnings
        .iter()
        .any(|warning| warning.ends_with("invalid port"))
    {
        engine = Err("one or more configured ports are invalid".into());
    }
    ImportedConnection {
        id: path.clone(),
        name: name.to_string(),
        description: field(&fields, "comment").to_string(),
        tags: vec![],
        folder: folder.replace('\\', "/"),
        engine,
        host: raw_host.to_string(),
        port: database_port,
        database,
        user: field(&fields, "user").to_string(),
        password: pass,
        stored_password: None,
        env: String::new(),
        read_only: false,
        ssh,
        proxy: None,
        tls,
        unmapped: warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(extra: &str) -> ImportedConnection {
        session_with_net_type(0, extra)
    }

    fn session_with_net_type(net: u8, extra: &str) -> ImportedConnection {
        let input = format!(
            "Servers\\Example\\Host<|||>1<|||>localhost\nServers\\Example\\NetType<|||>3<|||>{net}\n{extra}"
        );
        convert(parse_export(&input).expect("valid export")).remove(0)
    }

    #[test]
    fn heidisql_offers_the_registry_only_on_windows() {
        assert_eq!(heidisql_has_registry(), cfg!(windows));
        if !heidisql_has_registry() {
            assert!(matches!(
                read_heidisql_registry(),
                Err(HeidiSqlError::RegistryUnavailable)
            ));
        }
    }

    #[test]
    fn heidisql_real_export_keeps_folders_and_ssh() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/heidisql/heidisql-12.22.txt");
        let sessions = read_heidisql(&path).unwrap();
        assert_eq!(sessions.len(), 2);
        let local = sessions.iter().find(|s| s.name == "Local").unwrap();
        assert_eq!(local.folder, "Examples");
        assert_eq!(local.engine, Ok(Engine::Mysql));
        assert_eq!(local.port, Some(13306));
        let ssh = sessions
            .iter()
            .find(|s| s.name == "SSH")
            .unwrap()
            .ssh
            .as_ref()
            .unwrap();
        assert_eq!(ssh.host, "bastion.example.test");
        assert_eq!(ssh.port, Some(2222));
        assert_eq!(ssh.auth, SshAuth::Key);
    }

    #[test]
    fn heidisql_password_rejects_malformed_unicode_without_panicking() {
        for malformed in ["ñ", "a😀", "é0", "aé1", "0", "0000", "XX1", "FFé"] {
            assert!(password(malformed).is_err(), "{malformed:?}");
        }
        assert!(password("1").unwrap().is_none());
        assert_eq!(password("6263641").unwrap().unwrap().expose(), "abc");
        assert_eq!(password("00620063006410").unwrap().unwrap().expose(), "abc");
        assert_eq!(password("0062D83EDE0110").unwrap().unwrap().expose(), "a😀");
    }

    #[test]
    fn heidisql_bad_ports_cannot_fall_back_to_defaults() {
        for (field, value) in [
            ("Port", "word"),
            ("Port", "65536"),
            ("Port", "0"),
            ("SSHtunnelHostPort", "word"),
            ("SSHtunnelHostPort", "65536"),
            ("SSHtunnelPort", "65536"),
        ] {
            let extra = format!(
                "Servers\\Example\\SSHtunnelActive<|||>3<|||>1\nServers\\Example\\{field}<|||>1<|||>{value}\n"
            );
            assert!(
                session_with_net_type(2, &extra).engine.is_err(),
                "{field}={value}"
            );
        }
    }

    #[test]
    fn heidisql_mysql_preferred_tls_warns_about_plaintext() {
        let c = session(
            "Servers\\Example\\SSL_Active<|||>3<|||>1\nServers\\Example\\SSL_Verification<|||>3<|||>0\n",
        );
        assert_eq!(c.tls.unwrap().mode, TlsModeConfig::Prefer);
        assert!(c.unmapped.iter().any(|w| w.contains("plaintext")));
    }

    #[test]
    fn heidisql_tls_settings_for_other_engines_are_disclosed() {
        for net in [4, 10] {
            let c = session_with_net_type(net, "Servers\\Example\\SSL_Active<|||>3<|||>1\n");
            assert!(c.tls.is_none());
            assert!(
                c.unmapped
                    .iter()
                    .any(|w| w.contains("TLS") && w.contains("not imported"))
            );
        }
    }

    #[test]
    fn heidisql_zero_local_port_means_automatic() {
        let c = session_with_net_type(
            2,
            "Servers\\Example\\SSHtunnelActive<|||>3<|||>1\nServers\\Example\\SSHtunnelPort<|||>3<|||>0\n",
        );
        assert!(c.engine.is_ok());
        assert_eq!(c.ssh.unwrap().local_port, None);
    }

    #[test]
    fn heidisql_ssh_follows_the_source_transport_and_activation_flag() {
        let on = "Servers\\Example\\SSHtunnelActive<|||>3<|||>1\n";
        for net in [0, 10] {
            assert!(session_with_net_type(net, on).engine.is_err());
        }
        for net in [2, 4, 8, 9, 16] {
            let c = session_with_net_type(net, on);
            assert!(c.engine.is_ok(), "network type {net}");
            assert!(c.ssh.is_some(), "network type {net}");
        }
        for net in [2, 9, 16] {
            let c = session_with_net_type(net, "Servers\\Example\\SSHtunnelActive<|||>3<|||>0\n");
            assert!(c.engine.is_ok(), "network type {net}");
            assert!(c.ssh.is_none(), "network type {net}");
        }
        let c = session_with_net_type(
            2,
            "Servers\\Example\\SSHtunnelActive<|||>3<|||>0\nServers\\Example\\SSHtunnelPassword<|||>1<|||>é\n",
        );
        assert!(c.ssh.is_none());
        assert!(c.unmapped.iter().any(|w| w.contains("SSH is disabled")));
        assert!(!c.unmapped.iter().any(|w| w.contains("requested")));
    }

    #[test]
    fn heidisql_invalid_ssh_password_remains_a_password_prompt_in_the_import_plan() {
        let c = session_with_net_type(
            2,
            "Servers\\Example\\SSHtunnelActive<|||>3<|||>1\nServers\\Example\\SSHtunnelPassword<|||>1<|||>é\n",
        );
        assert_eq!(c.ssh.as_ref().unwrap().auth, SshAuth::Password);
        let plan = crate::import_plan(
            &c,
            Engine::Mysql,
            crate::Source::User,
            true,
            "test",
            &crate::TargetFolder {
                folder: String::new(),
                keep_client_folders: true,
            },
        );
        let (tunnel, _) = plan.tunnel.unwrap();
        assert_eq!(tunnel.hops[0].password, crate::PasswordChoice::Prompt);
    }

    #[cfg(windows)]
    #[test]
    fn heidisql_registry_reads_only_the_supplied_key_and_bounds_empty_keys() {
        use std::time::{SystemTime, UNIX_EPOCH};

        struct OwnKey {
            parent: windows_registry::Key,
            name: String,
        }
        impl Drop for OwnKey {
            fn drop(&mut self) {
                assert!(self.name.starts_with("SpelaionDB-HeidiSQL-test-"));
                self.parent
                    .remove_tree(&self.name)
                    .expect("remove own test key");
            }
        }
        let parent = windows_registry::CURRENT_USER
            .options()
            .read()
            .write()
            .open("Software")
            .unwrap();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("SpelaionDB-HeidiSQL-test-{}-{nonce}", std::process::id());
        assert!(parent.open(&name).is_err());
        let root = parent.create(&name).unwrap();
        let _cleanup = OwnKey { parent, name };
        let session = root.create(r"Folder\SSH").unwrap();
        session.set_string("Host", "db.example.test").unwrap();
        session.set_u32("NetType", 2).unwrap();
        session
            .set_string("SSHtunnelHost", "ssh.example.test")
            .unwrap();
        let result = convert(registry_sessions(&root).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].folder, "Folder");
        assert_eq!(result[0].ssh.as_ref().unwrap().host, "ssh.example.test");
        for i in 0..6 {
            root.create(format!("empty-{i}")).unwrap();
        }
        assert!(registry_sessions_bounded(&root, 5, MAX_BYTES).is_err());
        assert!(registry_sessions_bounded(&root, MAX_FIELDS, 5).is_err());
    }

    #[test]
    fn heidisql_unmapped_does_not_reveal_secret_values() {
        let c = session(
            "Servers\\Example\\UnknownToken<|||>1<|||>super-secret\nServers\\Example\\Password<|||>1<|||>é\n",
        );
        assert!(!format!("{c:?}").contains("super-secret"));
        assert!(c.unmapped.iter().any(|w| w.contains("unknowntoken")));
    }

    #[test]
    fn heidisql_unsafe_settings_fail_closed() {
        for extra in [
            "Servers\\Example\\WindowsAuth<|||>3<|||>1\n",
            "Servers\\Example\\SSL_Active<|||>3<|||>surprise\n",
            "Servers\\Example\\User<|||>1<|||>${env:SECRET}\n",
        ] {
            assert!(session(extra).engine.is_err());
        }
        for n in ["1", "3", "17", "not-a-type"] {
            let input = format!(
                "Servers\\Example\\Host<|||>1<|||>host\nServers\\Example\\NetType<|||>3<|||>{n}\n"
            );
            assert!(convert(parse_export(&input).unwrap())[0].engine.is_err());
        }
    }

    #[test]
    fn heidisql_supported_engine_types() {
        for (net, expected) in [
            (0, Engine::Mysql),
            (4, Engine::Sqlserver),
            (8, Engine::Postgres),
            (9, Engine::Postgres),
            (10, Engine::Sqlite),
        ] {
            let input = format!(
                "Servers\\Example\\Host<|||>1<|||>host\nServers\\Example\\NetType<|||>3<|||>{net}\n"
            );
            assert_eq!(
                convert(parse_export(&input).unwrap())[0].engine,
                Ok(expected)
            );
        }
        let c = session("Servers\\Example\\ServerVersionFull<|||>1<|||>10.11 MariaDB\n");
        assert_eq!(c.engine, Ok(Engine::Mariadb));
    }
}
