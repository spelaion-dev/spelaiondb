//! What every connection import has in common, whichever client the connections come from
//! (DBeaver, DataGrip, …): one model of an imported connection, the drafts SpelaionDB creates
//! for it, the target folder, and how a connection that already exists is found and replaced.
//!
//! A client's reader (`dbeaver.rs`, `datagrip.rs`) only turns that client's files into
//! [`ImportedConnection`]s; everything after that is here, so a new client plugs in by writing
//! a reader. Passwords only live as [`Secret`]s on their way to the keychain: they are never
//! shown, logged or written to a file.

use spelaion_driver_api::Secret;

use crate::secrets::SecretBackend;

use crate::draft::{ConnectionDraft, PasswordChoice, SessionDraft, folder_parts, normalize_tags};
use crate::load::{Config, ConnectionEntry, Source, check_tag, is_hidden_char};
use crate::model::{ConnectionFile, Engine, ProxyType, SshAuth, TlsModeConfig};
use crate::tunnel_draft::{HopDraft, ProxyDraft, TunnelDraft, TunnelDraftOptions, TunnelPasswords};

/// The clients SpelaionDB imports connections from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportClient {
    Dbeaver,
    Datagrip,
}

impl ImportClient {
    /// The id the UI uses.
    pub fn as_str(self) -> &'static str {
        match self {
            ImportClient::Dbeaver => "dbeaver",
            ImportClient::Datagrip => "datagrip",
        }
    }

    /// The client's name, as people write it.
    pub fn label(self) -> &'static str {
        match self {
            ImportClient::Dbeaver => "DBeaver",
            ImportClient::Datagrip => "DataGrip",
        }
    }
}

/// One connection read from another client, as far as SpelaionDB can use it.
#[derive(Debug, Clone)]
pub struct ImportedConnection {
    /// The client's own identifier, to pick connections to import.
    pub id: String,
    pub name: String,
    /// The client's description of the connection; empty for none.
    pub description: String,
    /// Labels: `name`, or `name=value` when the client gives a value.
    pub tags: Vec<String>,
    /// The client's folder, `a/b` for nested ones.
    pub folder: String,
    /// The engine, or why SpelaionDB cannot open it (yet).
    pub engine: Result<Engine, String>,
    pub host: String,
    pub port: Option<u16>,
    /// Database name; for SQLite and DuckDB, the file path.
    pub database: String,
    pub user: String,
    /// A password the reader already has (DBeaver's saved credentials).
    pub password: Option<Secret>,
    /// Where the client keeps the password in the system keychain, read only when importing
    /// with passwords (never while previewing); `None` when it keeps none there.
    pub stored_password: Option<KeychainItem>,
    /// The built-in connection type closest to the client's (`dev`, `test` or `prod`,
    /// ADR-029); empty for none.
    pub env: String,
    pub read_only: bool,
    pub ssh: Option<ImportedSsh>,
    /// A SOCKS proxy, alone or in front of the SSH tunnel.
    pub proxy: Option<ImportedProxy>,
    /// TLS settings; `None` keeps SpelaionDB's default.
    pub tls: Option<ImportedTls>,
    /// What the client had that SpelaionDB does not import, as `what: value` lines for the
    /// preview: nothing is dropped silently.
    pub unmapped: Vec<String>,
}

/// An entry another client wrote in the system keychain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainItem {
    pub service: String,
    pub account: String,
}

/// The SSH tunnel of an imported connection.
#[derive(Debug, Clone)]
pub struct ImportedSsh {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub auth: SshAuth,
    pub key_path: String,
    pub password: Option<Secret>,
    /// Seconds; `None` where the client left it unset.
    pub keepalive_interval: Option<u32>,
    pub keepalive_count: Option<u32>,
    /// Seconds.
    pub connect_timeout: Option<u32>,
    /// A fixed local port for the tunnel; `None` for a free one.
    pub local_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct ImportedProxy {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password: Option<Secret>,
}

/// TLS settings of an imported connection: the mode and the files, as paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedTls {
    pub mode: TlsModeConfig,
    pub ca: String,
    pub cert: String,
    pub key: String,
}

impl ImportedConnection {
    /// The name its imported network profile gets: one per SSH bastion and user (and proxy,
    /// if any), or per proxy alone, so connections that share one in the client share it here.
    pub fn tunnel_name(&self) -> Option<String> {
        let proxy = self
            .proxy
            .as_ref()
            .map(|p| format!("{} {}", p.host, p.port.unwrap_or(1080)));
        let slug = crate::draft::slug;
        match (&self.ssh, proxy) {
            (Some(ssh), None) => Some(slug(&format!("{} {}", ssh.user, ssh.host))),
            (Some(ssh), Some(proxy)) => {
                Some(slug(&format!("{} {} via {proxy}", ssh.user, ssh.host)))
            }
            (None, Some(proxy)) => Some(slug(&format!("socks {proxy}"))),
            (None, None) => None,
        }
    }

    /// Whether a password comes with it (read already, or from the keychain on import).
    pub fn has_password(&self) -> bool {
        self.password.is_some() || self.stored_password.is_some()
    }
}

/// Reads the passwords another client keeps in the system keychain into the connections
/// that have one ([`ImportedConnection::stored_password`]). Only called when the user imports
/// with passwords: on macOS the system asks before letting SpelaionDB read another app's
/// entry. An entry that is missing leaves the password to be asked when connecting; one that
/// cannot be read is returned with why, by connection name, and never stops the import.
pub fn read_stored_passwords(
    connections: &mut [ImportedConnection],
    backend: &dyn SecretBackend,
) -> Vec<(String, String)> {
    let mut failed = Vec::new();
    for connection in connections.iter_mut() {
        let Some(item) = &connection.stored_password else {
            continue;
        };
        match backend.other_app_keychain_get(&item.service, &item.account) {
            Ok(Some(secret)) if !secret.is_empty() => {
                connection.password = Some(Secret::new(secret))
            }
            Ok(_) => {}
            Err(message) => failed.push((connection.name.clone(), message)),
        }
    }
    failed
}

/// Where imported connections go in the explorer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetFolder {
    /// A folder to put them all in (`imported/dbeaver`); empty for the top level.
    pub folder: String,
    /// Keep the client's folders, under `folder`.
    pub keep_client_folders: bool,
}

impl TargetFolder {
    /// The folder a connection from `client_folder` goes to.
    pub fn for_connection(&self, client_folder: &str) -> String {
        let mut parts = vec![self.folder.trim().trim_matches('/').to_string()];
        if self.keep_client_folders {
            parts.push(client_folder.trim().trim_matches('/').to_string());
        }
        parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/")
            .to_lowercase()
    }

    /// Checks the typed folder before anything is written.
    pub fn check(&self) -> Result<(), String> {
        folder_parts(&self.folder)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// What to do with a connection that already exists in SpelaionDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateAction {
    /// Import it next to the existing one, under another name (`Orders (DBeaver)`).
    Import,
    /// Leave it out.
    Skip,
    /// Overwrite the existing connection's settings with the imported ones, keeping its name,
    /// folder and everything the client does not have.
    Replace,
}

/// The connection of `source` that an import of `connection` would duplicate: one with the
/// same name (ignoring case), or one that reaches the same database (same engine, host,
/// port, database and user; same file for SQLite and DuckDB).
pub fn find_duplicate<'a>(
    config: &'a Config,
    connection: &ImportedConnection,
    source: Source,
) -> Option<&'a ConnectionEntry> {
    let candidates = || config.connections.iter().filter(|c| c.id.source == source);
    let name = connection.name.trim().to_lowercase();
    if let Some(found) = candidates().find(|c| !name.is_empty() && c.name().to_lowercase() == name)
    {
        return Some(found);
    }
    let engine = *connection.engine.as_ref().ok()?;
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    candidates().find(|c| {
        let file = &c.file;
        if file.engine != engine {
            return false;
        }
        let database = file.database.as_deref().unwrap_or_default();
        if engine.is_file() {
            return !database.is_empty() && same_path(database, &connection.database);
        }
        let port = |p: Option<u16>| p.or(engine.default_port());
        same(file.host.as_deref().unwrap_or_default(), &connection.host)
            && port(file.port) == port(connection.port)
            && database == connection.database.trim()
            && same(file.user.as_deref().unwrap_or_default(), &connection.user)
    })
}

/// Two file paths that name the same file as far as the text goes (`\` or `/`, and case on
/// Windows).
fn same_path(a: &str, b: &str) -> bool {
    let normal = |p: &str| {
        let p = p.trim().replace('\\', "/");
        if cfg!(windows) { p.to_lowercase() } else { p }
    };
    normal(a) == normal(b)
}

/// The names an imported connection tries, in order, when its own is taken: `name`,
/// `name (DataGrip)`, `name (DataGrip 2)`…
pub fn import_names(name: &str, client: ImportClient) -> impl Iterator<Item = String> + '_ {
    (0..10).map(move |attempt| match attempt {
        0 => name.to_string(),
        1 => format!("{name} ({})", client.label()),
        n => format!("{name} ({} {n})", client.label()),
    })
}

/// What SpelaionDB will create for a connection: the connection, its tunnel (if any) and
/// the secrets that go to the keychain with them.
pub struct ImportPlan {
    pub connection: ConnectionDraft,
    pub password: Option<Secret>,
    pub tunnel: Option<(TunnelDraft, TunnelPasswords)>,
}

/// The drafts for one imported connection. With `save_passwords`, the passwords the client
/// saved go to the keychain; otherwise (or when it saved none) they are asked when
/// connecting. A password the client keeps in the keychain is read into
/// `connection.password` before this, and only when the user asked for passwords.
pub fn import_plan(
    connection: &ImportedConnection,
    engine: Engine,
    source: Source,
    save_passwords: bool,
    tunnel_name: &str,
    folder: &TargetFolder,
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
    let file = engine.is_file();
    let tls = connection.tls.clone().filter(|_| !file);
    ImportPlan {
        connection: ConnectionDraft {
            source,
            folder: folder.for_connection(&connection.folder),
            name: connection.name.clone(),
            description: importable_description(&connection.description),
            tags: importable_tags(&connection.tags),
            engine,
            env: connection.env.clone(),
            host: if file {
                String::new()
            } else {
                connection.host.clone()
            },
            port: if file { None } else { connection.port },
            database: connection.database.clone(),
            user: if file {
                String::new()
            } else {
                connection.user.clone()
            },
            password: if file {
                PasswordChoice::None
            } else {
                choice(&connection.password)
            },
            tls_mode: tls.as_ref().map(|t| t.mode).unwrap_or_default(),
            tls_ca: tls.as_ref().map(|t| t.ca.clone()).unwrap_or_default(),
            tls_cert: tls.as_ref().map(|t| t.cert.clone()).unwrap_or_default(),
            tls_key: tls.as_ref().map(|t| t.key.clone()).unwrap_or_default(),
            tunnel: if tunnel.is_some() {
                tunnel_name.to_string()
            } else {
                String::new()
            },
            local_port: connection.ssh.as_ref().and_then(|ssh| ssh.local_port),
            kubernetes: None,
            sqlite3: String::new(),
            read_only: connection.read_only,
            // Like a connection made in the form: idle sessions are kept alive (ADR-028).
            keepalive: (!file).then_some(ConnectionFile::SUGGESTED_KEEPALIVE),
            connect_timeout: None,
            statement_timeout: None,
            session: SessionDraft::default(),
            mcp_exposed: false,
            mcp_writes: false,
        },
        password: if file {
            None
        } else {
            keep(&connection.password)
        },
        tunnel,
    }
}

/// The draft that replaces `existing` with an imported connection: what the client has
/// (engine, address, user, password, TLS, tunnel) comes from `plan`; the existing name,
/// folder and everything the client has no say on (session setup, timeouts, agents' access,
/// Kubernetes) stay. A read-only connection stays read-only, and a description, tags or type
/// the client does not give are kept.
pub fn replacement_draft(existing: &ConnectionEntry, plan: &ConnectionDraft) -> ConnectionDraft {
    let mut draft = ConnectionDraft::from_entry(existing);
    draft.engine = plan.engine;
    draft.host = plan.host.clone();
    draft.port = plan.port;
    draft.database = plan.database.clone();
    draft.user = plan.user.clone();
    draft.password = plan.password.clone();
    draft.tls_mode = plan.tls_mode;
    draft.tls_ca = plan.tls_ca.clone();
    draft.tls_cert = plan.tls_cert.clone();
    draft.tls_key = plan.tls_key.clone();
    draft.tunnel = plan.tunnel.clone();
    draft.local_port = plan.local_port;
    // The imported tunnel takes over from a Kubernetes forward; without one, it stays.
    if !plan.tunnel.is_empty() {
        draft.kubernetes = None;
    }
    draft.read_only |= plan.read_only;
    if !plan.description.is_empty() {
        draft.description = plan.description.clone();
    }
    if !plan.tags.is_empty() {
        draft.tags = plan.tags.clone();
    }
    if !plan.env.is_empty() {
        draft.env = plan.env.clone();
    }
    draft
}

/// A description as SpelaionDB accepts it: without hidden characters and cut to the longest
/// description, so one long note does not stop the connection from being imported.
pub(crate) fn importable_description(description: &str) -> String {
    description
        .chars()
        .filter(|c| !is_hidden_char(*c) || matches!(c, '\n' | '\r' | '\t'))
        .take(ConnectionFile::MAX_DESCRIPTION)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Tags as SpelaionDB accepts them: commas become spaces, hidden characters go, and a tag
/// that is still too long, or one past the most a connection holds, is left out, so a tag
/// does not stop the connection from being imported.
pub(crate) fn importable_tags(tags: &[String]) -> Vec<String> {
    let cleaned: Vec<String> = tags
        .iter()
        .map(|tag| {
            tag.chars()
                .filter(|c| !is_hidden_char(*c) || c.is_whitespace())
                .map(|c| if c == ',' { ' ' } else { c })
                .collect()
        })
        .collect();
    normalize_tags(&cleaned)
        .into_iter()
        .filter(|tag| check_tag(tag).is_ok())
        .take(ConnectionFile::MAX_TAGS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_are_imported_as_spelaiondb_accepts_them() {
        assert_eq!(
            importable_description("  Sales\r\n\tby region "),
            "Sales\r\n\tby region"
        );
        // Escapes and bidirectional overrides are dropped; the text around them stays.
        assert_eq!(
            importable_description("safe\u{1b}[2J text \u{202e}desrever"),
            "safe[2J text desrever"
        );
        let long = "x".repeat(ConnectionFile::MAX_DESCRIPTION + 50);
        assert_eq!(
            importable_description(&long).chars().count(),
            ConnectionFile::MAX_DESCRIPTION
        );
    }

    #[test]
    fn tags_are_imported_as_spelaiondb_accepts_them() {
        let tags: Vec<String> = [
            " eu ",
            "EU",
            "a,b",
            "hidden\u{202e}mark",
            "",
            "far too long for a tag of a connection",
            "multi \t word",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // Repeats ignoring case, empty and over-long tags are left out; the rest is cleaned.
        assert_eq!(
            importable_tags(&tags),
            ["eu", "a b", "hiddenmark", "multi word"]
        );
        let many: Vec<String> = (0..30).map(|i| format!("t{i}")).collect();
        assert_eq!(importable_tags(&many).len(), ConnectionFile::MAX_TAGS);
        // What is imported is a valid connection.
        let draft_tags = importable_tags(&tags);
        assert!(
            draft_tags.iter().all(|t| check_tag(t).is_ok()),
            "{draft_tags:?}"
        );
    }
}
