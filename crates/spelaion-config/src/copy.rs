//! Copies of a connection (ADR-030): "Duplicate" in the explorer, and Ctrl+C / Ctrl+V.
//!
//! - **Duplicate** copies the file next to the original, in the same layer and folder, with
//!   every setting, comment and hand-written key, and only a new name. A password stored in
//!   the keychain under the original's entry is copied to the copy's own entry here, in the
//!   core: it never reaches the UI. Without a system keychain the copy asks for it, as the
//!   original does; a keychain that exists but fails is an error and nothing is created. A repository connection is duplicated only while it is as
//!   the user trusted it (ADR-022), and the copy is recorded as the user's own write.
//! - **Copy** puts a text form of the connection on the clipboard: the connection file without
//!   anything that reads a secret or runs a program. **Paste** reads such a text (or a
//!   connection URL) as untrusted input: it becomes a personal connection through the same
//!   checks as the connection form, whose password is always asked when connecting, which
//!   runs no command and which agents cannot use until the user turns that on.

use std::path::Path;

use spelaion_driver_api::Secret;

use crate::accounts::connection_account;
use crate::conn_url::parse_connection_url;
use crate::draft::{
    ConnectionDraft, add_connection, connections_dir, put, slug, uses_default_keychain,
};
use crate::edit::{CONNECTION_SCHEMA_URL, read_doc};
use crate::error::{ConfigError, ResolveError};
use crate::load::{Config, ConnectionEntry, ConnectionId, Source, validate_connection};
use crate::model::{ConnectionFile, Engine, McpConfig, SCHEMA_VERSION, SecretSpec};
use crate::resolve::Resolver;
use crate::secrets::{SecretBackend, forget};
use crate::trust::TrustStore;

/// A connection made by [`duplicate_connection`].
#[derive(Debug, Clone)]
pub struct Duplicated {
    pub id: ConnectionId,
    pub name: String,
    pub file: ConnectionFile,
    /// The original's password was in the keychain and the copy has its own entry now.
    pub password_copied: bool,
}

/// The clipboard form of a connection, from [`connection_clipboard_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardConnection {
    pub text: String,
    /// Settings left out of the text (see [`paste_connection`]), by their file names.
    pub left_out: Vec<String>,
}

/// A connection made by [`paste_connection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pasted {
    pub id: ConnectionId,
    pub name: String,
    /// Settings of the text that were not taken, by their file names: `password` (asked
    /// when connecting instead), `kubernetes.kubeconfig`, `kubernetes.kubectl`, `sqlite3`,
    /// `mcp`, `tunnel` (no tunnel of that name here).
    pub left_out: Vec<String>,
}

/// Longest clipboard text a paste reads.
pub const MAX_CLIPBOARD_TEXT: usize = 64 * 1024;

/// What the errors of a paste are about: the text, not a file of the user's.
const CLIPBOARD: &str = "clipboard";

/// An error checking pasted text, said of the clipboard rather than of the file it would
/// have become (whose path means nothing to the user, and shows their folders).
fn from_clipboard(error: ConfigError) -> ResolveError {
    ConfigError::new(Path::new(CLIPBOARD), error.message).into()
}

/// First line of the clipboard form.
const CLIPBOARD_HEADER: &str =
    "# A SpelaionDB connection, without secrets: its password is asked when connecting.";

fn missing(id: &ConnectionId) -> ConfigError {
    ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
}

/// The original's file name and its folders (`prod/eu/orders` → `orders`, `prod/eu/`).
fn split_id(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(at) => (&path[at + 1..], &path[..=at]),
        None => (path, ""),
    }
}

/// Duplicates a connection next to the original: `<file>-copy` (then `-copy-2`…) named
/// "<name> (copy)" (then "(copy 2)"…). See the module documentation for secrets and trust.
pub fn duplicate_connection(
    config: &Config,
    trust: &mut TrustStore,
    id: &ConnectionId,
    backend: &dyn SecretBackend,
) -> Result<Duplicated, ResolveError> {
    let entry = config.connection(id).ok_or_else(|| missing(id))?;
    // An untrusted, new or changed repository connection is never copied: the copy would be
    // recorded as trusted.
    Resolver::new(config, trust, backend).check_editor_access(id.source, Some(id))?;
    let path = &entry.file_path;
    let (original, mut doc) = read_doc(path)?;
    let on_disk: ConnectionFile = toml::from_str(&original)
        .map_err(|e| ConfigError::new(path, format!("could not read connection: {e}")))?;
    if validate_connection(path, on_disk)? != entry.file {
        return Err(ConfigError::new(path, "the connection changed; refresh the explorer").into());
    }
    let dir = connections_dir(config, id.source)?;
    let (stem, folders) = split_id(&id.path);
    let base = entry.file.name.clone().unwrap_or_else(|| stem.to_string());

    for n in 1..=999u32 {
        let (suffix, label) = match n {
            1 => ("-copy".to_string(), "copy".to_string()),
            n => (format!("-copy-{n}"), format!("copy {n}")),
        };
        let copy = ConnectionId {
            source: id.source,
            path: format!("{folders}{stem}{suffix}"),
        };
        let file_path = dir.join(format!("{}.toml", copy.path));
        if config.connection(&copy).is_some() || file_path.exists() {
            continue;
        }
        let name = format!("{base} ({label})");
        put(
            &mut doc,
            &["name"],
            Some(toml_edit::Value::from(name.as_str())),
        );
        let text = doc.to_string();
        let parsed: ConnectionFile = toml::from_str(&text)
            .map_err(|e| ConfigError::new(&file_path, format!("the copy is not valid: {e}")))?;
        let file = validate_connection(&file_path, parsed)?;
        // A new file, never one over another (a copy never replaces a connection), with the
        // original's mode.
        match crate::atomic_file::create_like(&file_path, text.as_bytes(), &entry.file_path) {
            Ok(()) => {}
            // Someone else took this name in the meantime: the next one.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(ConfigError::new(&file_path, e.to_string()).into()),
        }
        let undo = |error: ResolveError| {
            let _ = std::fs::remove_file(&file_path);
            error
        };

        let mut password_copied = false;
        let to = connection_account(config, &copy);
        // Without a system keychain there is nothing to copy: the copy asks, as the original.
        if uses_default_keychain(&file) && backend.keychain_available() {
            let from = connection_account(config, id);
            let stored = match backend.keychain_get(&from) {
                Ok(Some(secret)) => {
                    let secret = Secret::new(secret);
                    password_copied = true;
                    backend.keychain_set(&to, secret.expose())
                }
                // Nothing to copy: the copy asks. An entry left under the copy's name by an
                // older connection must not be sent to this copy's server.
                Ok(None) => forget(backend, &to),
                // A keychain that exists but fails (locked, refused) is an error.
                Err(message) => Err(message),
            };
            if let Err(message) = stored {
                return Err(undo(ResolveError::Secret {
                    reference: format!("keychain:{to}"),
                    message,
                }));
            }
        }
        if id.source == Source::Workspace {
            // The user made it through the app, from a connection they trust: theirs.
            if let Err(e) = trust.accept_connection_write(config, &copy, &file) {
                if password_copied {
                    let _ = forget(backend, &to);
                }
                return Err(undo(e.into()));
            }
        }
        return Ok(Duplicated {
            id: copy,
            name,
            file,
            password_copied,
        });
    }
    Err(ConfigError::new(path, "too many copies of this connection").into())
}

/// What a copy leaves out: anything that reads a secret, runs a program or lets agents in.
fn sanitize(mut file: ConnectionFile) -> (ConnectionFile, Vec<String>) {
    let mut left_out = Vec::new();
    if let Some(password) = &file.password
        && password.secret != "prompt"
    {
        left_out.push("password".to_string());
        file.password = Some(SecretSpec {
            secret: "prompt".into(),
        });
    }
    if let Some(kubernetes) = &mut file.kubernetes {
        // A kubeconfig can run programs of its own (`exec` credentials).
        if kubernetes.kubeconfig.take().is_some() {
            left_out.push("kubernetes.kubeconfig".to_string());
        }
        if kubernetes.kubectl.take().is_some() {
            left_out.push("kubernetes.kubectl".to_string());
        }
    }
    if file.sqlite3.take().is_some() {
        left_out.push("sqlite3".to_string());
    }
    // Setup statements run on the database every time a session opens (ADR-037): pasted text
    // must not bring SQL along. The default schema and isolation level only choose settings.
    if file.session.setup.take().is_some() {
        left_out.push("session.setup".to_string());
        file.session.ignore_setup_errors = false;
    }
    if file.mcp != McpConfig::default() {
        left_out.push("mcp".to_string());
        file.mcp = McpConfig::default();
    }
    (file, left_out)
}

/// A path relative to the repository root, made absolute so it means the same elsewhere.
fn absolute(path: &mut Option<String>, root: &Path) {
    if let Some(value) = path
        && Path::new(value.as_str()).is_relative()
        && !value.starts_with("~/")
        && !value.contains("${")
    {
        *value = root.join(value.as_str()).to_string_lossy().into_owned();
    }
}

/// The clipboard form of a connection: its file without secrets or programs (see
/// [`paste_connection`]), with paths of a repository connection made absolute. Like
/// duplicating, a repository connection must be as the user trusted it.
pub fn connection_clipboard_text(
    config: &Config,
    trust: &TrustStore,
    id: &ConnectionId,
    backend: &dyn SecretBackend,
) -> Result<ClipboardConnection, ResolveError> {
    let entry = config.connection(id).ok_or_else(|| missing(id))?;
    Resolver::new(config, trust, backend).check_editor_access(id.source, Some(id))?;
    let mut file = entry.file.clone();
    if id.source == Source::Workspace
        && let Some(root) = &config.workspace_root
    {
        let remote_sqlite = file.engine == Engine::Sqlite && file.tunnel.is_some();
        if file.engine.is_file() && !remote_sqlite {
            absolute(&mut file.database, root);
        }
        absolute(&mut file.tls.ca, root);
        absolute(&mut file.tls.cert, root);
        absolute(&mut file.tls.key, root);
    }
    if file.name.is_none() {
        file.name = Some(split_id(&id.path).0.to_string());
    }
    let (file, left_out) = sanitize(file);
    let body = toml::to_string_pretty(&file)
        .map_err(|e| ConfigError::new(&entry.file_path, e.to_string()))?;
    Ok(ClipboardConnection {
        text: format!("{CLIPBOARD_HEADER}\n#:schema {CONNECTION_SCHEMA_URL}\n\n{body}"),
        left_out,
    })
}

/// A connection URL as the table of a connection file. Its password is never kept.
fn url_table(text: &str) -> Option<toml::Table> {
    let url = parse_connection_url(text).ok()?;
    let mut table = toml::Table::new();
    let mut set = |key: &str, value: toml::Value| {
        table.insert(key.into(), value);
    };
    set(
        "schema_version",
        toml::Value::Integer(i64::from(SCHEMA_VERSION)),
    );
    set("engine", url.engine.as_str().into());
    let name = if url.engine.is_file() {
        let file = url.database.rsplit(['/', '\\']).next().unwrap_or_default();
        file.rsplit_once('.')
            .map_or(file, |(stem, _)| stem)
            .to_string()
    } else if url.database.is_empty() {
        url.host.clone()
    } else {
        url.database.clone()
    };
    if !name.trim().is_empty() {
        set("name", name.trim().into());
    }
    for (key, value) in [
        ("host", &url.host),
        ("database", &url.database),
        ("user", &url.user),
    ] {
        if !value.is_empty() && (key == "database" || !url.engine.is_file()) {
            set(key, value.as_str().into());
        }
    }
    if let Some(port) = url.port.filter(|_| !url.engine.is_file()) {
        set("port", toml::Value::Integer(i64::from(port)));
    }
    if url.password.is_some() && !url.engine.is_file() {
        let mut secret = toml::Table::new();
        secret.insert("secret".into(), "prompt".into());
        set("password", toml::Value::Table(secret));
    }
    if let Some(mode) = url.tls_mode.filter(|_| !url.engine.is_file()) {
        let mut tls = toml::Table::new();
        tls.insert(
            "mode".into(),
            toml::Value::try_from(mode).unwrap_or_else(|_| "prefer".into()),
        );
        set("tls", toml::Value::Table(tls));
    }
    Some(table)
}

/// Every `${…}` variable used in any string of the value.
fn variables(value: &toml::Value, out: &mut Vec<String>) {
    match value {
        toml::Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(at) = rest.find("${") {
                let after = &rest[at + 2..];
                let end = after.find('}').unwrap_or(after.len());
                out.push(after[..end].to_string());
                rest = &after[end..];
            }
        }
        toml::Value::Array(items) => items.iter().for_each(|v| variables(v, out)),
        toml::Value::Table(table) => table.values().for_each(|v| variables(v, out)),
        toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => {}
    }
}

/// Creates a personal connection from clipboard text: the form of [`connection_clipboard_text`]
/// or a connection URL. The text is untrusted (it may come from anywhere), so:
///
/// - it must be a valid connection file, checked exactly as the connection form checks one;
/// - its password, whatever the text says, is asked when connecting: nothing is read from the
///   keychain, the environment, 1Password or a command;
/// - nothing runs a program: `cmd:` passwords and cloud tokens, `kubernetes.kubectl`,
///   `kubernetes.kubeconfig` (which can run its own) and `sqlite3` are left out;
/// - no SQL comes along: the session setup statements are left out (ADR-037);
/// - `${env:…}` is refused: resolving it would send an environment variable to the server
///   (or to DNS through the host name);
/// - agents cannot use it (`[mcp]` is left out), and a tunnel is kept only if one of that
///   name exists here.
///
/// It goes to the personal connections (never the repository), with a new name if the
/// text's is taken.
pub fn paste_connection(
    config: &Config,
    text: &str,
    backend: &dyn SecretBackend,
) -> Result<Pasted, ResolveError> {
    let here = config.user_dir.join("connections");
    let error =
        |message: String| ResolveError::from(ConfigError::new(Path::new(CLIPBOARD), message));
    if text.len() > MAX_CLIPBOARD_TEXT {
        return Err(error(
            "the clipboard text is too long for a connection".into(),
        ));
    }
    let trimmed = text.trim();
    let unknown = || {
        error(
            "the clipboard does not hold a connection: copy one in SpelaionDB, or a connection URL"
                .into(),
        )
    };
    if trimmed.is_empty() {
        return Err(unknown());
    }
    let table = match url_table(trimmed).filter(|_| !trimmed.contains('\n')) {
        Some(table) => table,
        None => trimmed.parse::<toml::Table>().map_err(|_| unknown())?,
    };
    let value = toml::Value::Table(table);
    let mut names = Vec::new();
    variables(&value, &mut names);
    if let Some(name) = names.iter().find(|n| !n.starts_with("user.")) {
        return Err(error(format!(
            "a pasted connection cannot use ${{{name}}}: only ${{user.…}} variables are allowed"
        )));
    }
    let file: ConnectionFile = value.try_into().map_err(|e| {
        error(format!(
            "the clipboard does not hold a valid connection: {e}"
        ))
    })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(error(format!(
            "the pasted connection is format version {}; this SpelaionDB reads version {SCHEMA_VERSION}",
            file.schema_version
        )));
    }
    let (mut file, mut left_out) = sanitize(file);
    if let Some(tunnel) = &file.tunnel
        && config.tunnel_for(Source::User, tunnel).is_none()
    {
        left_out.push("tunnel".to_string());
        file.tunnel = None;
        file.local_port = None;
    }
    let base = file
        .name
        .clone()
        .filter(|n| !n.trim().is_empty() && !slug(n).is_empty())
        .unwrap_or_else(|| "Pasted connection".to_string());
    let base = base.trim().to_string();
    let mut draft = ConnectionDraft::from_entry(&ConnectionEntry {
        id: ConnectionId {
            source: Source::User,
            path: slug(&base),
        },
        file_path: here.join("pasted.toml"),
        file,
    });
    draft.folder.clear();
    for n in 1..=999u32 {
        draft.name = match n {
            1 => base.clone(),
            2 => format!("{base} (copy)"),
            n => format!("{base} (copy {})", n - 1),
        };
        let id = draft.id().map_err(from_clipboard)?;
        if config.connection(&id).is_some() || here.join(format!("{}.toml", id.path)).exists() {
            continue;
        }
        // The form's own checks and writing; no password is typed, so the keychain is not
        // touched.
        let id = add_connection(config, &draft, None, backend).map_err(|e| match e {
            ResolveError::Config(e) => from_clipboard(e),
            other => other,
        })?;
        return Ok(Pasted {
            id,
            name: draft.name,
            left_out,
        });
    }
    Err(error("too many connections with this name".into()))
}
