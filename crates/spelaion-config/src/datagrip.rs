//! Connections imported from DataGrip (and the other JetBrains IDEs with its Database
//! plugin): a project's `.idea/dataSources.xml` and `dataSources.local.xml`, the IDE's global
//! data sources (`options/dataSources*.xml` in its configuration folder) and its SSH
//! configurations (`options/sshConfigs.xml`, or a project's `.idea/sshConfigs.xml`).
//!
//! DataGrip keeps passwords out of those files. On macOS, with its "In native Keychain"
//! setting, they are in the keychain under `IntelliJ Platform DB — <data source id>`: they are
//! only read when the user imports with passwords ([`crate::read_stored_passwords`]). On
//! Windows DataGrip only offers its own KeePass database (2026.3 has no native option), and on
//! Linux its Secret Service items are not searchable by this store: those are not read, and the
//! preview says so.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::conn_url::parse_connection_url;
use crate::import::{ImportedConnection, ImportedSsh, ImportedTls, KeychainItem};
use crate::model::{Engine, SshAuth, TlsModeConfig};

/// The keychain service DataGrip saves a data source's password under (JetBrains'
/// `generateServiceName("DB", id)`).
fn keychain_service(id: &str) -> String {
    format!("IntelliJ Platform DB — {id}")
}

/// The JetBrains products whose configuration folders may hold data sources: DataGrip and
/// the IDEs that bundle its Database plugin.
const PRODUCTS: &[&str] = &[
    "DataGrip",
    "IntelliJIdea",
    "PyCharm",
    "WebStorm",
    "PhpStorm",
    "GoLand",
    "RubyMine",
    "Rider",
    "CLion",
    "RustRover",
];

#[derive(Debug, thiserror::Error)]
pub enum DatagripError {
    #[error(
        "no DataGrip data sources under {0} (a project with .idea/dataSources.xml, or a JetBrains configuration folder)"
    )]
    NotFound(PathBuf),
    #[error("could not read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
}

fn unreadable(path: &Path, e: impl std::fmt::Display) -> DatagripError {
    DatagripError::Unreadable {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// Where JetBrains keeps its configuration folders on this system (`…/JetBrains`; it may not
/// exist).
pub fn jetbrains_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("JetBrains"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/JetBrains"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|d| d.join("JetBrains"))
    }
}

/// The configuration folders under a JetBrains folder that have data sources or SSH
/// configurations, the newest version of each product first, so a data source copied from an
/// older version is read from the newer one.
pub fn datagrip_dirs(jetbrains: &Path) -> Vec<PathBuf> {
    let mut found: Vec<(String, Vec<u32>, PathBuf)> = std::fs::read_dir(jetbrains)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let product = PRODUCTS.iter().find(|p| name.starts_with(**p))?;
            let version: Vec<u32> = name[product.len()..]
                .split('.')
                .map(|n| n.parse().ok())
                .collect::<Option<_>>()?;
            let path = entry.path();
            path.join("options")
                .is_dir()
                .then(|| (product.to_string(), version, path))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    found.into_iter().map(|(_, _, path)| path).collect()
}

/// A tiny XML tree: enough to read JetBrains' files.
#[derive(Debug, Default)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
    text: String,
}

impl Node {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The trimmed text of a child, or of an attribute of that name (JetBrains writes some
    /// settings either way).
    fn value(&self, name: &str) -> String {
        self.child(name)
            .map(|c| c.text.trim().to_string())
            .or_else(|| self.attr(name).map(|v| v.trim().to_string()))
            .unwrap_or_default()
    }

    fn flag(&self, name: &str) -> bool {
        self.value(name).eq_ignore_ascii_case("true")
    }

    /// Every element named `name` at any depth.
    fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        for child in &self.children {
            if child.name == name {
                out.push(child);
            }
            child.find_all(name, out);
        }
    }
}

fn parse_xml(path: &Path) -> Result<Option<Node>, DatagripError> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    parse_text(&content)
        .map(Some)
        .map_err(|e| unreadable(path, e))
}

fn parse_text(content: &str) -> Result<Node, String> {
    let mut reader = Reader::from_str(content);
    let mut stack = vec![Node::default()];
    let open = |e: &quick_xml::events::BytesStart<'_>| -> Result<Node, String> {
        let mut node = Node {
            name: e.name().as_ref().to_string(),
            ..Node::default()
        };
        for attr in e.attributes() {
            let attr = attr.map_err(|e| e.to_string())?;
            let value = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|e| e.to_string())?;
            node.attrs
                .push((attr.key.as_ref().to_string(), value.to_string()));
        }
        Ok(node)
    };
    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) => stack.push(open(&e)?),
            Event::Empty(e) => {
                let node = open(&e)?;
                stack.last_mut().expect("root").children.push(node);
            }
            Event::End(_) => {
                let node = stack.pop().expect("open element");
                stack
                    .last_mut()
                    .ok_or("unbalanced XML")?
                    .children
                    .push(node);
            }
            Event::Text(t) => stack
                .last_mut()
                .expect("root")
                .text
                .push_str(&t.xml10_content()),
            Event::CData(t) => stack.last_mut().expect("root").text.push_str(&t),
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref().map_err(|e| e.to_string())? {
                    Some(c) => c.to_string(),
                    None => match r.xml10_content().as_ref() {
                        "amp" => "&".into(),
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "quot" => "\"".into(),
                        "apos" => "'".into(),
                        other => format!("&{other};"),
                    },
                };
                stack.last_mut().expect("root").text.push_str(&resolved);
            }
            Event::Eof => break,
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) | Event::DocType(_) => {}
        }
    }
    if stack.len() != 1 {
        return Err("unbalanced XML".into());
    }
    Ok(stack.pop().expect("root"))
}

/// Where a set of data sources lives: a project's `.idea`, or a configuration folder's
/// `options` (global data sources).
struct Location {
    /// The folder holding `dataSources.xml` and `dataSources.local.xml`.
    dir: PathBuf,
    /// The project folder (what `$PROJECT_DIR$` means), for a project.
    project: Option<PathBuf>,
}

/// The data sources under `dir` (a project, its `.idea`, a JetBrains configuration folder, or
/// the JetBrains folder itself), with SSH configurations looked up in the project and in
/// `config_dirs` (the IDEs' configuration folders, newest first).
pub fn read_datagrip(
    dir: &Path,
    config_dirs: &[PathBuf],
) -> Result<Vec<ImportedConnection>, DatagripError> {
    let mut configs: Vec<PathBuf> = Vec::new();
    let mut locations: Vec<Location> = Vec::new();
    let has_sources =
        |d: &Path| d.join("dataSources.xml").is_file() || d.join("dataSources.local.xml").is_file();
    let project_at = |idea: &Path| Location {
        dir: idea.to_path_buf(),
        project: idea.parent().map(Path::to_path_buf),
    };
    if dir.file_name().is_some_and(|n| n == ".idea") && has_sources(dir) {
        locations.push(project_at(dir));
    } else if has_sources(&dir.join(".idea")) {
        locations.push(project_at(&dir.join(".idea")));
    } else if dir.join("options").is_dir() {
        configs.push(dir.to_path_buf());
    } else {
        configs.extend(datagrip_dirs(dir));
    }
    for config in &configs {
        let options = config.join("options");
        if has_sources(&options) {
            locations.push(Location {
                dir: options.clone(),
                project: None,
            });
        }
        for project in recent_projects(config)? {
            let idea = project.join(".idea");
            if has_sources(&idea) && !locations.iter().any(|l| l.dir == idea) {
                locations.push(project_at(&idea));
            }
        }
    }
    if locations.is_empty() {
        return Err(DatagripError::NotFound(dir.to_path_buf()));
    }
    let mut ssh_dirs: Vec<PathBuf> = configs.iter().map(|c| c.join("options")).collect();
    ssh_dirs.extend(config_dirs.iter().map(|c| c.join("options")));
    let mut out: Vec<ImportedConnection> = Vec::new();
    for location in &locations {
        let mut search = Vec::new();
        if location.project.is_some() {
            search.push(location.dir.clone());
        }
        search.extend(ssh_dirs.iter().cloned());
        let ssh = ssh_configs(&search)?;
        let owner = location.dir.parent().map(Path::to_path_buf);
        let keychain = keychain_mode(if location.project.is_some() {
            config_dirs.first().or(configs.first())
        } else {
            owner.as_ref()
        });
        for connection in read_location(location, &ssh, keychain)? {
            // The same data source in an older version's folder, or in two projects that
            // share it, is listed once.
            if !out.iter().any(|c| c.id == connection.id) {
                out.push(connection);
            }
        }
    }
    Ok(out)
}

/// The projects a JetBrains configuration folder lists as recent (`options/recentProjects.xml`).
fn recent_projects(config: &Path) -> Result<Vec<PathBuf>, DatagripError> {
    let Some(root) = parse_xml(&config.join("options").join("recentProjects.xml"))? else {
        return Ok(Vec::new());
    };
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut entries = Vec::new();
    root.find_all("entry", &mut entries);
    Ok(entries
        .into_iter()
        .filter_map(|e| e.attr("key"))
        .map(|key| {
            let expanded = key.replace("$USER_HOME$", &home.to_string_lossy());
            PathBuf::from(expanded)
        })
        .collect())
}

/// How the IDE keeps passwords: in the system keychain, or not (its KeePass database, or
/// nowhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeychainMode {
    Native,
    KeePass,
    None,
}

/// The IDE's password storage (`options/security.xml`, `PasswordSafe`'s `PROVIDER`). Without
/// the setting, JetBrains' default: KeePass on Windows, the system keychain elsewhere.
fn keychain_mode(config: Option<&PathBuf>) -> KeychainMode {
    let provider = config
        .and_then(|c| {
            parse_xml(&c.join("options").join("security.xml"))
                .ok()
                .flatten()
        })
        .and_then(|root| {
            let mut options = Vec::new();
            root.find_all("option", &mut options);
            options
                .into_iter()
                .find(|o| o.attr("name") == Some("PROVIDER"))
                .and_then(|o| o.attr("value").map(str::to_uppercase))
        });
    match provider.as_deref() {
        Some("KEYCHAIN") => KeychainMode::Native,
        Some("KEEPASS") => KeychainMode::KeePass,
        Some(_) => KeychainMode::None,
        None if cfg!(windows) => KeychainMode::KeePass,
        None => KeychainMode::Native,
    }
}

/// An SSH configuration of the IDE.
#[derive(Debug, Clone)]
struct SshConfig {
    host: String,
    port: Option<u16>,
    user: String,
    auth: String,
    key_path: String,
    keepalive: Option<u32>,
    timeout: Option<u32>,
    unmapped: Vec<String>,
}

/// The SSH configurations by id, from the first folder that defines each one.
fn ssh_configs(dirs: &[PathBuf]) -> Result<HashMap<String, SshConfig>, DatagripError> {
    let mut out = HashMap::new();
    for dir in dirs {
        let Some(root) = parse_xml(&dir.join("sshConfigs.xml"))? else {
            continue;
        };
        let mut configs = Vec::new();
        root.find_all("sshConfig", &mut configs);
        for config in configs {
            let Some(id) = config.attr("id") else {
                continue;
            };
            let number = |name: &str| config.value(name).parse::<u32>().ok().filter(|n| *n > 0);
            let mut unmapped = Vec::new();
            if config.flag("useOpenSSHConfig") {
                unmapped.push("SSH: DataGrip also reads ~/.ssh/config for this host; SpelaionDB uses only the settings imported here".into());
            }
            if !config.value("proxyParams").is_empty() || config.child("proxyParams").is_some() {
                unmapped.push("SSH: proxy of the SSH connection".into());
            }
            out.entry(id.to_string()).or_insert(SshConfig {
                host: config.value("host"),
                port: config.value("port").parse().ok(),
                user: config.value("username"),
                auth: config.value("authType").to_uppercase(),
                key_path: config.value("keyPath"),
                keepalive: number("keepAliveInterval"),
                timeout: number("connectionTimeout"),
                unmapped,
            });
        }
    }
    Ok(out)
}

/// The settings DataGrip writes for itself (caches, introspection) or that this module
/// reads: never listed as unmapped.
const KNOWN: &[&str] = &[
    "driver-ref",
    "synchronize",
    "jdbc-driver",
    "jdbc-url",
    "working-dir",
    "comment",
    "database-info",
    "case-sensitivity",
    "secret-storage",
    "user-name",
    "schema-mapping",
    "introspection-schemas",
    "introspection-scope",
    "introspection-level",
    "ssh-properties",
    "ssl-config",
    "auth-provider",
    "auth-required",
    "read-only",
    "data-source-type",
    "imported",
    "remarks",
    "libraries",
];

/// Reads one `dataSources.xml` + `dataSources.local.xml` pair.
fn read_location(
    location: &Location,
    ssh: &HashMap<String, SshConfig>,
    keychain: KeychainMode,
) -> Result<Vec<ImportedConnection>, DatagripError> {
    let shared = parse_xml(&location.dir.join("dataSources.xml"))?;
    let local = parse_xml(&location.dir.join("dataSources.local.xml"))?;
    fn sources(root: &Option<Node>) -> Vec<&Node> {
        let mut found = Vec::new();
        if let Some(root) = root {
            root.find_all("data-source", &mut found);
        }
        found
    }
    let shared_sources = sources(&shared);
    let local_sources = sources(&local);
    let mut out = Vec::new();
    for source in &shared_sources {
        let Some(id) = source.attr("uuid") else {
            continue;
        };
        let local = local_sources
            .iter()
            .copied()
            .find(|l| l.attr("uuid") == Some(id));
        out.push(connection_of(id, source, local, location, ssh, keychain));
    }
    Ok(out)
}

/// `$PROJECT_DIR$` and `$USER_HOME$` in a path written by DataGrip.
fn expand(path: &str, location: &Location) -> String {
    let mut path = path.trim().to_string();
    if let Some(project) = &location.project {
        path = path.replace("$PROJECT_DIR$", &project.to_string_lossy());
        path = path.replace("$ProjectFileDir$", &project.to_string_lossy());
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        path = path.replace("$USER_HOME$", &home.to_string_lossy());
    }
    path
}

fn engine_of(driver: &str, url: &str) -> Result<Engine, String> {
    let driver = driver.to_lowercase();
    let scheme = url
        .trim()
        .to_lowercase()
        .strip_prefix("jdbc:")
        .and_then(|rest| rest.split(':').next().map(str::to_string))
        .unwrap_or_default();
    let is = |prefixes: &[&str]| {
        prefixes
            .iter()
            .any(|p| driver.starts_with(p) || scheme == p.trim_end_matches('.'))
    };
    if is(&["postgresql"]) {
        Ok(Engine::Postgres)
    } else if is(&["mariadb"]) {
        Ok(Engine::Mariadb)
    } else if is(&["mysql"]) {
        Ok(Engine::Mysql)
    } else if is(&["sqlserver", "azure.ms"]) {
        Ok(Engine::Sqlserver)
    } else if is(&["sqlite"]) {
        Ok(Engine::Sqlite)
    } else if is(&["duckdb"]) {
        Ok(Engine::Duckdb)
    } else {
        let what = if driver.is_empty() { scheme } else { driver };
        Err(format!("{what} is not supported yet"))
    }
}

/// A setting's value as the preview may show it: a key that may hold a secret shows its
/// name only.
fn shown(name: &str, value: &str) -> String {
    let lower = name.to_lowercase();
    if ["password", "secret", "token", "key", "credential"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return format!("{name} (value not shown)");
    }
    let value: String = value.chars().filter(|c| !c.is_control()).take(80).collect();
    if value.is_empty() {
        name.to_string()
    } else {
        format!("{name}={value}")
    }
}

fn connection_of(
    id: &str,
    source: &Node,
    local: Option<&Node>,
    location: &Location,
    ssh: &HashMap<String, SshConfig>,
    keychain: KeychainMode,
) -> ImportedConnection {
    let url = source.value("jdbc-url");
    let engine = engine_of(&source.value("driver-ref"), &url);
    let mut unmapped = Vec::new();
    let parsed = if url.is_empty() {
        None
    } else {
        match parse_connection_url(&url) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                if engine.is_ok() {
                    unmapped.push(format!("URL: {e}"));
                }
                None
            }
        }
    };
    if parsed.as_ref().is_some_and(|p| p.password.is_some()) {
        unmapped.push("URL: a password in the URL (not imported)".into());
    }
    // Settings in the URL's query that SpelaionDB has no field for.
    if let Some(query) = url.split_once('?').map(|(_, q)| q) {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if !matches!(
                key.to_lowercase().as_str(),
                "sslmode" | "ssl" | "user" | "password"
            ) {
                unmapped.push(format!("URL: {}", shown(key, value)));
            }
        }
    }
    for child in &source.children {
        let name = child.name.as_str();
        if KNOWN.contains(&name) {
            continue;
        }
        match name {
            "jdbc-additional-properties" | "driver-properties" => {
                for property in child.children("property") {
                    let key = property.attr("name").unwrap_or_default();
                    let value = property.attr("value").unwrap_or_default();
                    unmapped.push(format!("Driver property: {}", shown(key, value)));
                }
            }
            other => unmapped.push(format!(
                "DataGrip setting: {}",
                shown(other, child.text.trim())
            )),
        }
    }
    let user = local
        .map(|l| l.value("user-name"))
        .filter(|u| !u.is_empty())
        .or_else(|| parsed.as_ref().map(|p| p.user.clone()))
        .unwrap_or_default();
    // DataGrip keeps a saved password in its password safe ("Save: Forever" writes
    // `master_key`; "Until restart" and "Never" keep none on disk).
    let saved = local.is_some_and(|l| {
        matches!(
            l.value("secret-storage").to_lowercase().as_str(),
            "master_key" | "forever"
        )
    });
    let file = engine.as_ref().is_ok_and(|e| e.is_file());
    let stored_password = match (saved && !file, keychain) {
        (true, KeychainMode::Native) if cfg!(target_os = "macos") => Some(KeychainItem {
            service: keychain_service(id),
            account: user.clone(),
        }),
        (true, KeychainMode::Native) => {
            unmapped.push("Password: if DataGrip saved one, it is in the system keychain, which SpelaionDB only reads from other applications on macOS; it is asked when connecting".into());
            None
        }
        (true, KeychainMode::KeePass) => {
            unmapped.push("Password: if DataGrip saved one, it is in its KeePass database, which SpelaionDB does not read; it is asked when connecting".into());
            None
        }
        (true, KeychainMode::None) | (false, _) => None,
    };
    let tls = local.and_then(|l| tls_of(l, location, &mut unmapped));
    let tls = tls.or_else(|| {
        parsed
            .as_ref()
            .and_then(|p| p.tls_mode)
            .map(|mode| ImportedTls {
                mode,
                ca: String::new(),
                cert: String::new(),
                key: String::new(),
            })
    });
    let ssh = local.and_then(|l| ssh_of(l, ssh, &mut unmapped));
    let mut database = parsed
        .as_ref()
        .map(|p| p.database.clone())
        .unwrap_or_default();
    if file {
        database = expand(&database, location);
    }
    ImportedConnection {
        id: id.to_string(),
        name: source.attr("name").unwrap_or_default().trim().to_string(),
        description: source.value("comment"),
        tags: Vec::new(),
        folder: source.attr("group").unwrap_or_default().trim().to_string(),
        engine,
        host: parsed.as_ref().map(|p| p.host.clone()).unwrap_or_default(),
        port: parsed.as_ref().and_then(|p| p.port),
        database,
        user,
        password: None,
        stored_password,
        env: String::new(),
        read_only: source.flag("read-only"),
        ssh,
        proxy: None,
        tls,
        unmapped,
    }
}

fn tls_of(local: &Node, location: &Location, unmapped: &mut Vec<String>) -> Option<ImportedTls> {
    let ssl = local.child("ssl-config")?;
    if !ssl.flag("enabled") {
        return None;
    }
    let mode = match ssl.value("mode").to_uppercase().as_str() {
        "VERIFY_CA" => TlsModeConfig::VerifyCa,
        "VERIFY_FULL" => TlsModeConfig::VerifyFull,
        "PREFER" => TlsModeConfig::Prefer,
        "DISABLE" => TlsModeConfig::Disable,
        "REQUIRE" | "" => TlsModeConfig::Require,
        other => {
            unmapped.push(format!("TLS: mode {other} (imported as Require)"));
            TlsModeConfig::Require
        }
    };
    let path = |name: &str| expand(&ssl.value(name), location);
    Some(ImportedTls {
        mode,
        ca: path("ca-cert"),
        cert: path("client-cert"),
        key: path("client-key"),
    })
}

fn ssh_of(
    local: &Node,
    configs: &HashMap<String, SshConfig>,
    unmapped: &mut Vec<String>,
) -> Option<ImportedSsh> {
    let properties = local.child("ssh-properties")?;
    if !properties.flag("enabled") {
        return None;
    }
    let id = properties.value("ssh-config-id");
    let Some(config) = configs.get(&id) else {
        unmapped.push(
            "SSH: its SSH configuration was not found (it may be in another IDE's settings)".into(),
        );
        return None;
    };
    // DataGrip leaves `authType` out for its default, a key pair (as it writes it for one
    // picked by hand).
    let auth = match config.auth.as_str() {
        "KEY_PAIR" => SshAuth::Key,
        "" if !config.key_path.is_empty() => SshAuth::Key,
        "OPEN_SSH" | "AGENT" => SshAuth::Agent,
        "PASSWORD" | "" => SshAuth::Password,
        other => {
            unmapped.push(format!(
                "SSH: authentication {other} (imported as password)"
            ));
            SshAuth::Password
        }
    };
    unmapped.extend(config.unmapped.iter().cloned());
    Some(ImportedSsh {
        host: config.host.clone(),
        port: config.port,
        user: config.user.clone(),
        auth,
        key_path: if auth == SshAuth::Key {
            config.key_path.clone()
        } else {
            String::new()
        },
        password: None,
        keepalive_interval: config
            .keepalive
            .map(|s| s.min(crate::model::TunnelFile::MAX_KEEPALIVE_INTERVAL)),
        keepalive_count: None,
        connect_timeout: config
            .timeout
            .map(|s| s.min(crate::model::TunnelFile::MAX_CONNECT_TIMEOUT)),
        local_port: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_read_with_entities() {
        let root = parse_text(r#"<a x="1 &amp; 2"><b>caf&#233; &lt;3&gt;</b><c/></a>"#).unwrap();
        let a = root.child("a").unwrap();
        assert_eq!(a.attr("x"), Some("1 & 2"));
        assert_eq!(a.value("b"), "café <3>");
        assert!(a.child("c").is_some());
        assert!(parse_text("<a><b></a>").is_err());
    }

    #[test]
    fn engines_come_from_the_driver_or_the_url() {
        assert_eq!(engine_of("postgresql", ""), Ok(Engine::Postgres));
        assert_eq!(engine_of("mysql.8", ""), Ok(Engine::Mysql));
        assert_eq!(engine_of("mariadb", ""), Ok(Engine::Mariadb));
        assert_eq!(engine_of("sqlserver.ms", ""), Ok(Engine::Sqlserver));
        assert_eq!(engine_of("azure.ms", ""), Ok(Engine::Sqlserver));
        assert_eq!(engine_of("sqlite.xerial", ""), Ok(Engine::Sqlite));
        assert_eq!(engine_of("duckdb", ""), Ok(Engine::Duckdb));
        assert_eq!(
            engine_of("", "jdbc:postgresql://h/db"),
            Ok(Engine::Postgres)
        );
        assert!(engine_of("oracle", "jdbc:oracle:thin:@h:1521:x").is_err());
    }

    #[test]
    fn secret_looking_settings_show_their_name_only() {
        assert_eq!(shown("sslpassword", "x"), "sslpassword (value not shown)");
        assert_eq!(shown("ApplicationName", "app"), "ApplicationName=app");
        assert_eq!(shown("flag", ""), "flag");
    }
}
