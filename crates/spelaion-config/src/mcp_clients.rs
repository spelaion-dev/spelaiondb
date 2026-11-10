//! Installing `spelaiondb mcp` in AI clients (ADR-033): where each client keeps its MCP
//! servers, whether ours is there, and the change that adds it, shown before it is written.
//!
//! Only our own entry (`spelaiondb`) is touched, and only its `command` and `args` (and
//! `type` for Claude Code): every other byte of the file stays as it was, so the user's
//! formatting, comments (TOML) and other servers survive. JSON files are edited as text at
//! the spans a small scanner finds, after `serde_json` has checked the file; Codex's TOML goes
//! through `toml_edit`. A file that does not parse is never written: the user edits it by
//! hand. Before writing, the file is read again and must be exactly what the shown change was
//! made from; the old file is kept as `<file>.spelaion-backup-<date>`. The new file and the
//! backup keep the old file's mode (owner-only for a new file), and a symbolic link is
//! followed, so the link stays a link (`atomic_file`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::atomic_file::{self, NewFileMode};

/// The name of our entry in every client's file.
pub const SERVER_NAME: &str = "spelaiondb";

/// Points every client file under this directory instead of the user's (tests and the smoke
/// test: the user's real files are never touched there).
pub const CLIENTS_HOME_ENV: &str = "SPELAION_MCP_CLIENTS_HOME";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpClient {
    ClaudeDesktop,
    ClaudeCode,
    Codex,
    Cursor,
    Windsurf,
    Antigravity,
}

impl McpClient {
    pub const ALL: [McpClient; 6] = [
        McpClient::ClaudeDesktop,
        McpClient::ClaudeCode,
        McpClient::Codex,
        McpClient::Cursor,
        McpClient::Windsurf,
        McpClient::Antigravity,
    ];

    pub fn id(self) -> &'static str {
        match self {
            McpClient::ClaudeDesktop => "claude-desktop",
            McpClient::ClaudeCode => "claude-code",
            McpClient::Codex => "codex",
            McpClient::Cursor => "cursor",
            McpClient::Windsurf => "windsurf",
            McpClient::Antigravity => "antigravity",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            McpClient::ClaudeDesktop => "Claude Desktop",
            McpClient::ClaudeCode => "Claude Code",
            McpClient::Codex => "Codex",
            McpClient::Cursor => "Cursor",
            McpClient::Windsurf => "Windsurf",
            McpClient::Antigravity => "Antigravity",
        }
    }

    fn toml(self) -> bool {
        match self {
            McpClient::Codex => true,
            McpClient::ClaudeDesktop
            | McpClient::ClaudeCode
            | McpClient::Cursor
            | McpClient::Windsurf
            | McpClient::Antigravity => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

/// Where the user's files are: the system's directories, or a pretend home for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientEnv {
    pub os: Os,
    pub home: PathBuf,
    /// Windows: `%APPDATA%` (Roaming).
    pub app_data: Option<PathBuf>,
    /// Windows: `%LOCALAPPDATA%`, where the Microsoft Store's Claude Desktop keeps its copy.
    pub local_app_data: Option<PathBuf>,
    /// Linux: `$XDG_CONFIG_HOME`.
    pub xdg_config: Option<PathBuf>,
    /// `$CODEX_HOME`, if set.
    pub codex_home: Option<PathBuf>,
    /// `$CLAUDE_CONFIG_DIR`, if set: Claude Code keeps `.claude.json` there.
    pub claude_config_dir: Option<PathBuf>,
}

impl ClientEnv {
    /// The user's directories, or [`CLIENTS_HOME_ENV`]'s pretend home if it is set.
    pub fn system() -> Option<ClientEnv> {
        let os = Os::current();
        if let Some(root) = std::env::var_os(CLIENTS_HOME_ENV).filter(|v| !v.is_empty()) {
            return Some(ClientEnv::under(Path::new(&root), os));
        }
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = match os {
            Os::Windows => var("USERPROFILE").or_else(|| var("HOME")),
            Os::MacOs | Os::Linux => var("HOME"),
        }?;
        Some(ClientEnv {
            os,
            home,
            app_data: var("APPDATA"),
            local_app_data: var("LOCALAPPDATA"),
            xdg_config: var("XDG_CONFIG_HOME"),
            codex_home: var("CODEX_HOME"),
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
        })
    }

    /// Every directory under `root`, laid out as on `os`.
    pub fn under(root: &Path, os: Os) -> ClientEnv {
        let windows = os == Os::Windows;
        ClientEnv {
            os,
            home: root.to_path_buf(),
            app_data: windows.then(|| root.join("AppData").join("Roaming")),
            local_app_data: windows.then(|| root.join("AppData").join("Local")),
            xdg_config: None,
            codex_home: None,
            claude_config_dir: None,
        }
    }
}

/// The file where `client` reads its MCP servers.
pub fn config_path(client: McpClient, env: &ClientEnv) -> PathBuf {
    let home = &env.home;
    match client {
        McpClient::ClaudeDesktop => claude_desktop_dir(env).join("claude_desktop_config.json"),
        McpClient::ClaudeCode => env
            .claude_config_dir
            .clone()
            .unwrap_or_else(|| home.clone())
            .join(".claude.json"),
        McpClient::Codex => env
            .codex_home
            .clone()
            .unwrap_or_else(|| home.join(".codex"))
            .join("config.toml"),
        McpClient::Cursor => home.join(".cursor").join("mcp.json"),
        McpClient::Windsurf => home
            .join(".codeium")
            .join("windsurf")
            .join("mcp_config.json"),
        McpClient::Antigravity => home
            .join(".gemini")
            .join("antigravity")
            .join("mcp_config.json"),
    }
}

/// Claude Desktop's folder: `%APPDATA%\Claude` on Windows (or the Microsoft Store package's
/// private copy of it, which that version reads first), `~/Library/Application Support/Claude`
/// on macOS, and `~/.config/Claude` on Linux (unofficial builds).
fn claude_desktop_dir(env: &ClientEnv) -> PathBuf {
    match env.os {
        Os::Windows => {
            if let Some(local) = &env.local_app_data
                && let Some(store) = store_package_dir(&local.join("Packages"))
            {
                return store;
            }
            env.app_data
                .clone()
                .unwrap_or_else(|| env.home.join("AppData").join("Roaming"))
                .join("Claude")
        }
        Os::MacOs => env
            .home
            .join("Library")
            .join("Application Support")
            .join("Claude"),
        Os::Linux => env
            .xdg_config
            .clone()
            .unwrap_or_else(|| env.home.join(".config"))
            .join("Claude"),
    }
}

/// `Packages\Claude_<id>\LocalCache\Roaming\Claude`, if the Microsoft Store's Claude Desktop
/// has made it.
fn store_package_dir(packages: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(packages)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("claude_")
        })
        .map(|e| e.path().join("LocalCache").join("Roaming").join("Claude"))
        .filter(|dir| dir.is_dir())
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The command a client runs to start our server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCommand {
    pub command: String,
    pub args: Vec<String>,
}

impl ServerCommand {
    /// `<binary> mcp`.
    pub fn new(binary: &Path) -> ServerCommand {
        ServerCommand {
            command: binary.display().to_string(),
            args: vec!["mcp".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpClientError {
    #[error("{path} cannot be read: {message}")]
    Read { path: PathBuf, message: String },
    #[error("{path} is not valid {format}, so it is not changed; fix it by hand ({message})")]
    Parse {
        path: PathBuf,
        format: &'static str,
        message: String,
    },
    #[error("{path}: {message}")]
    Shape { path: PathBuf, message: String },
    #[error("{path} changed since the change was shown; look at it again")]
    Changed { path: PathBuf },
    #[error("{path} cannot be written: {message}")]
    Write { path: PathBuf, message: String },
}

/// Whether a client already runs our server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallState {
    /// The file does not exist yet.
    NoFile,
    /// The file exists without our entry.
    NotInstalled,
    /// Our entry runs this command.
    Installed,
    /// Our entry is there with another command (an older path, other arguments).
    Different,
    /// The file cannot be read or parsed: it is left alone.
    Unreadable(String),
}

fn read(path: &Path) -> Result<Option<String>, McpClientError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).map_err(|_| McpClientError::Read {
                path: path.to_path_buf(),
                message: "it is not UTF-8 text".into(),
            })?;
            // A byte order mark is kept out of the parsers and put back on write.
            Ok(Some(text))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(McpClientError::Read {
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
    }
}

/// Whether `client` (whose file is at `path`) runs `command` as our server.
pub fn install_state(client: McpClient, path: &Path, command: &ServerCommand) -> InstallState {
    let text = match read(path) {
        Ok(Some(text)) => text,
        Ok(None) => return InstallState::NoFile,
        Err(e) => return InstallState::Unreadable(e.to_string()),
    };
    let body = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if body.trim().is_empty() {
        return InstallState::NotInstalled;
    }
    let entry = if client.toml() {
        match body.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => doc
                .get("mcp_servers")
                .and_then(|s| s.as_table_like())
                .and_then(|s| s.get(SERVER_NAME))
                .map(|entry| {
                    let entry = entry.as_table_like();
                    let command_of = entry
                        .and_then(|e| e.get("command"))
                        .and_then(|c| c.as_str())
                        .map(str::to_string);
                    let args_of = entry
                        .and_then(|e| e.get("args"))
                        .and_then(|a| a.as_array())
                        .map(|a| {
                            a.iter()
                                .map(|v| v.as_str().unwrap_or_default().to_string())
                                .collect::<Vec<_>>()
                        });
                    (command_of, args_of)
                }),
            Err(e) => return InstallState::Unreadable(parse_error(path, "TOML", e).to_string()),
        }
    } else {
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(json) => json
                .get("mcpServers")
                .and_then(|s| s.get(SERVER_NAME))
                .map(|entry| {
                    let command_of = entry
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(str::to_string);
                    let args_of = entry.get("args").and_then(|a| a.as_array()).map(|a| {
                        a.iter()
                            .map(|v| v.as_str().unwrap_or_default().to_string())
                            .collect::<Vec<_>>()
                    });
                    (command_of, args_of)
                }),
            Err(e) => return InstallState::Unreadable(parse_error(path, "JSON", e).to_string()),
        }
    };
    match entry {
        None => InstallState::NotInstalled,
        Some((Some(c), Some(a))) if c == command.command && a == command.args => {
            InstallState::Installed
        }
        Some(_) => InstallState::Different,
    }
}

fn parse_error(path: &Path, format: &'static str, e: impl std::fmt::Display) -> McpClientError {
    McpClientError::Parse {
        path: path.to_path_buf(),
        format,
        message: e.to_string().lines().next().unwrap_or_default().to_string(),
    }
}

/// One line of the change shown before writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Same(String),
    Added(String),
    Removed(String),
    /// Unchanged lines left out between two changes.
    Gap(usize),
}

/// The change that installs our server in one client, computed but not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub client: McpClient,
    pub path: PathBuf,
    /// The file as it was read; `None` if it does not exist.
    pub before: Option<String>,
    pub after: String,
}

impl InstallPlan {
    /// Nothing would change: our entry is already there as wanted.
    pub fn unchanged(&self) -> bool {
        self.before.as_deref() == Some(self.after.as_str())
    }

    /// The change, with three unchanged lines around each part.
    pub fn diff(&self) -> Vec<DiffLine> {
        line_diff(self.before.as_deref().unwrap_or_default(), &self.after, 3)
    }

    /// Identifies this exact change, so the change written is the change the user saw.
    pub fn fingerprint(&self) -> String {
        // FNV-1a: stable across runs and versions, which `DefaultHasher` does not promise.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        };
        feed(self.path.to_string_lossy().as_bytes());
        feed(&[u8::from(self.before.is_some())]);
        feed(self.before.as_deref().unwrap_or_default().as_bytes());
        feed(self.after.as_bytes());
        format!("{hash:016x}")
    }
}

/// The change that makes `client`'s file at `path` run `command` as our server.
pub fn plan_install(
    client: McpClient,
    path: &Path,
    command: &ServerCommand,
) -> Result<InstallPlan, McpClientError> {
    let before = read(path)?;
    let (bom, body) = match before.as_deref() {
        Some(text) => match text.strip_prefix('\u{feff}') {
            Some(rest) => ("\u{feff}", rest),
            None => ("", text),
        },
        None => ("", ""),
    };
    let after_body = if client.toml() {
        toml_install(path, body, command)?
    } else {
        json_install(client, path, body, command)?
    };
    Ok(InstallPlan {
        client,
        path: path.to_path_buf(),
        before,
        after: format!("{bom}{after_body}"),
    })
}

/// Writes `plan` if the file is still what it was made from, keeping a backup of the old
/// file. Returns the backup's path (`None` when there was no file).
pub fn apply_install(plan: &InstallPlan) -> Result<Option<PathBuf>, McpClientError> {
    let path = &plan.path;
    let write_error = |e: std::io::Error| McpClientError::Write {
        path: path.clone(),
        message: e.to_string(),
    };
    if read(path)? != plan.before {
        return Err(McpClientError::Changed { path: path.clone() });
    }
    if plan.unchanged() {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(write_error)?;
    }
    // These files hold tokens (other servers' `env`, the client's own login): the backup and
    // the new file keep the old file's mode, and a new file is owner-only. The backup sits
    // beside the path the client reads, even when that is a link into a dotfiles repository,
    // so it never lands in that repository.
    let backup = match &plan.before {
        Some(before) => {
            let backup = backup_path(path, SystemTime::now());
            atomic_file::create_like(&backup, before.as_bytes(), path).map_err(write_error)?;
            Some(backup)
        }
        None => None,
    };
    // Written beside the file (the link's target for a link), then moved over it: a client
    // never reads half a file, and a link stays a link.
    atomic_file::replace(path, plan.after.as_bytes(), NewFileMode::OwnerOnly)
        .map_err(write_error)?;
    Ok(backup)
}

/// Installs our server in `client` only if the change is still the one the user was shown
/// (`fingerprint`, from [`InstallPlan::fingerprint`]): the file is read again and the change
/// computed again, and anything different is refused. Returns the backup's path.
pub fn apply_shown(
    client: McpClient,
    path: &Path,
    command: &ServerCommand,
    fingerprint: &str,
) -> Result<Option<PathBuf>, McpClientError> {
    let plan = plan_install(client, path, command)?;
    if plan.fingerprint() != fingerprint {
        return Err(McpClientError::Changed {
            path: path.to_path_buf(),
        });
    }
    apply_install(&plan)
}

/// `<file>.spelaion-backup-<YYYYMMDD-HHMMSS>` (UTC), with `-2`, `-3`… if it exists.
fn backup_path(path: &Path, now: SystemTime) -> PathBuf {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    let stamp = format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        s / 3600,
        s % 3600 / 60,
        s % 60
    );
    let base = format!("{}.spelaion-backup-{stamp}", path.display());
    let mut candidate = PathBuf::from(&base);
    let mut n = 2;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

/// The date of a day counted from 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

// --- What the user copies by hand ----------------------------------------------------------

fn json_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serializes")
}

/// Our entry's fields, in order, as JSON values on one line each.
fn json_fields(client: McpClient, command: &ServerCommand) -> Vec<(&'static str, String)> {
    let mut fields = Vec::new();
    if client == McpClient::ClaudeCode {
        fields.push(("type", json_string("stdio")));
    }
    fields.push(("command", json_string(&command.command)));
    let args = command
        .args
        .iter()
        .map(|a| json_string(a))
        .collect::<Vec<_>>()
        .join(", ");
    fields.push(("args", format!("[{args}]")));
    fields
}

/// Our entry as a multi-line JSON object whose closing brace is at `indent`.
fn json_entry(client: McpClient, command: &ServerCommand, indent: &str, unit: &str) -> String {
    let inner = format!("{indent}{unit}");
    let fields = json_fields(client, command)
        .into_iter()
        .map(|(key, value)| format!("{inner}{}: {value}", json_string(key)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n{fields}\n{indent}}}")
}

/// `{"spelaiondb": <entry>}` as a multi-line object whose closing brace is at `indent`.
fn json_servers(client: McpClient, command: &ServerCommand, indent: &str, unit: &str) -> String {
    let inner = format!("{indent}{unit}");
    format!(
        "{{\n{inner}{}: {}\n{indent}}}",
        json_string(SERVER_NAME),
        json_entry(client, command, &inner, unit)
    )
}

/// The whole configuration for `client`, to copy into its file by hand.
pub fn manual_config(client: McpClient, command: &ServerCommand) -> String {
    if client.toml() {
        let mut doc = toml_edit::DocumentMut::new();
        set_toml_entry(&mut doc, command).expect("a new document has room for our entry");
        return doc.to_string();
    }
    format!(
        "{{\n  \"mcpServers\": {}\n}}\n",
        json_servers(client, command, "  ", "  ")
    )
}

/// The `claude mcp add` command that installs our server in Claude Code for every project.
pub fn claude_code_command(command: &ServerCommand, os: Os) -> String {
    let quote = |s: &str| {
        if !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:\\".contains(c))
        {
            s.to_string()
        } else {
            match os {
                Os::Windows => format!("\"{}\"", s.replace('"', "\\\"")),
                Os::MacOs | Os::Linux => format!("'{}'", s.replace('\'', "'\\''")),
            }
        }
    };
    let mut out = format!(
        "claude mcp add --scope user {SERVER_NAME} -- {}",
        quote(&command.command)
    );
    for arg in &command.args {
        out.push(' ');
        out.push_str(&quote(arg));
    }
    out
}

// --- Codex: TOML ---------------------------------------------------------------------------

fn set_toml_entry(doc: &mut toml_edit::DocumentMut, command: &ServerCommand) -> Result<(), String> {
    use toml_edit::{Array, Item, Table, value};
    let servers = doc.entry("mcp_servers").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    let servers = servers
        .as_table_like_mut()
        .ok_or("mcp_servers is not a table")?;
    let mut args = Array::new();
    for arg in &command.args {
        args.push(arg.as_str());
    }
    match servers.get_mut(SERVER_NAME) {
        Some(entry) if entry.is_table_like() => {
            // Only our keys change: the user's `env`, timeouts… stay.
            let entry = entry.as_table_like_mut().expect("checked to be table-like");
            entry.insert("command", value(command.command.as_str()));
            entry.insert("args", value(args));
        }
        _ => {
            let mut table = Table::new();
            table.insert("command", value(command.command.as_str()));
            table.insert("args", value(args));
            servers.insert(SERVER_NAME, Item::Table(table));
        }
    }
    Ok(())
}

fn toml_install(
    path: &Path,
    body: &str,
    command: &ServerCommand,
) -> Result<String, McpClientError> {
    let mut doc = body
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| parse_error(path, "TOML", e))?;
    set_toml_entry(&mut doc, command).map_err(|message| McpClientError::Shape {
        path: path.to_path_buf(),
        message,
    })?;
    let mut out = doc.to_string();
    let crlf = body.contains("\r\n");
    if crlf {
        // toml_edit writes new lines with `\n`: match the file's.
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    Ok(out)
}

// --- JSON: edited as text ------------------------------------------------------------------

/// A member of a JSON object, by byte offsets into the text.
struct Member {
    key: String,
    key_start: usize,
    value_start: usize,
    value_end: usize,
}

/// A JSON object: its braces and members.
struct Object {
    open: usize,
    close: usize,
    members: Vec<Member>,
}

impl Object {
    /// The last member with this key (the one JSON parsers keep).
    fn member(&self, key: &str) -> Option<&Member> {
        self.members.iter().rev().find(|m| m.key == key)
    }
}

/// Reads the structure of text `serde_json` already accepted.
struct Scanner<'a> {
    text: &'a str,
}

impl Scanner<'_> {
    fn ws(&self, mut i: usize) -> usize {
        let bytes = self.text.as_bytes();
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        i
    }

    /// A string starting at its opening quote: its value and the offset after it.
    fn string(&self, start: usize) -> Option<(String, usize)> {
        let bytes = self.text.as_bytes();
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => {
                    let value = serde_json::from_str(&self.text[start..=i]).ok()?;
                    return Some((value, i + 1));
                }
                _ => i += 1,
            }
        }
        None
    }

    /// The offset after the value starting at `start`.
    fn value_end(&self, start: usize) -> Option<usize> {
        let bytes = self.text.as_bytes();
        match *bytes.get(start)? {
            b'"' => self.string(start).map(|(_, end)| end),
            b'{' | b'[' => {
                let mut depth = 0usize;
                let mut i = start;
                while i < bytes.len() {
                    match bytes[i] {
                        b'"' => {
                            i = self.string(i)?.1;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(i + 1);
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                None
            }
            _ => {
                let mut i = start;
                while i < bytes.len()
                    && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
                {
                    i += 1;
                }
                Some(i)
            }
        }
    }

    fn object(&self, open: usize) -> Option<Object> {
        let bytes = self.text.as_bytes();
        if bytes.get(open) != Some(&b'{') {
            return None;
        }
        let mut members = Vec::new();
        let mut i = self.ws(open + 1);
        if bytes.get(i) == Some(&b'}') {
            return Some(Object {
                open,
                close: i,
                members,
            });
        }
        loop {
            let key_start = i;
            let (key, after_key) = self.string(i)?;
            i = self.ws(after_key);
            if bytes.get(i) != Some(&b':') {
                return None;
            }
            let value_start = self.ws(i + 1);
            let value_end = self.value_end(value_start)?;
            members.push(Member {
                key,
                key_start,
                value_start,
                value_end,
            });
            i = self.ws(value_end);
            match bytes.get(i)? {
                b',' => i = self.ws(i + 1),
                b'}' => {
                    return Some(Object {
                        open,
                        close: i,
                        members,
                    });
                }
                _ => return None,
            }
        }
    }

    /// The whitespace that starts the line holding `offset`, if only whitespace precedes it.
    fn line_indent(&self, offset: usize) -> Option<&str> {
        let line_start = self.text[..offset].rfind('\n').map_or(0, |n| n + 1);
        let before = &self.text[line_start..offset];
        before
            .chars()
            .all(|c| c == ' ' || c == '\t')
            .then_some(before)
    }
}

/// A replacement of `start..end` in the original text.
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// The indentation step of a file: the root's first member's, or two spaces.
fn indent_unit(scan: &Scanner, root: &Object) -> String {
    root.members
        .first()
        .and_then(|m| scan.line_indent(m.key_start))
        .filter(|indent| !indent.is_empty())
        .unwrap_or("  ")
        .to_string()
}

/// Adds a member to `object`, after its last one, at its members' indentation. `value`
/// renders the value given the member's indentation.
fn insert_member(
    scan: &Scanner,
    object: &Object,
    key: &str,
    unit: &str,
    value: impl Fn(&str) -> String,
) -> Edit {
    // The indentation of the line that opens the object (which may start with its key).
    let own_indent: String = {
        let line_start = scan.text[..object.open].rfind('\n').map_or(0, |n| n + 1);
        scan.text[line_start..object.open]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    };
    match object.members.last() {
        None => {
            let indent = format!("{own_indent}{unit}");
            Edit {
                start: object.open + 1,
                end: object.close,
                text: format!(
                    "\n{indent}{}: {}\n{own_indent}",
                    json_string(key),
                    value(&indent)
                ),
            }
        }
        Some(last) => {
            let indent = object
                .members
                .first()
                .and_then(|m| scan.line_indent(m.key_start))
                .map(str::to_string);
            match indent {
                Some(indent) => Edit {
                    start: last.value_end,
                    end: last.value_end,
                    text: format!(",\n{indent}{}: {}", json_string(key), value(&indent)),
                },
                // Members on the brace's line: keep it on one line.
                None => Edit {
                    start: last.value_end,
                    end: last.value_end,
                    text: format!(", {}: {}", json_string(key), value(&own_indent)),
                },
            }
        }
    }
}

fn shape(path: &Path, message: &str) -> McpClientError {
    McpClientError::Shape {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn json_install(
    client: McpClient,
    path: &Path,
    body: &str,
    command: &ServerCommand,
) -> Result<String, McpClientError> {
    if body.trim().is_empty() {
        return Ok(manual_config(client, command));
    }
    serde_json::from_str::<serde_json::Value>(body).map_err(|e| parse_error(path, "JSON", e))?;
    let scan = Scanner { text: body };
    let start = scan.ws(0);
    let root = scan
        .object(start)
        .ok_or_else(|| shape(path, "the file is not a JSON object"))?;
    let unit = indent_unit(&scan, &root);
    let mut edits = Vec::new();
    match root.member("mcpServers") {
        None => edits.push(insert_member(&scan, &root, "mcpServers", &unit, |indent| {
            json_servers(client, command, indent, &unit)
        })),
        Some(servers) => {
            let servers = scan
                .object(servers.value_start)
                .ok_or_else(|| shape(path, "mcpServers is not an object"))?;
            match servers.member(SERVER_NAME) {
                None => edits.push(insert_member(
                    &scan,
                    &servers,
                    SERVER_NAME,
                    &unit,
                    |indent| json_entry(client, command, indent, &unit),
                )),
                Some(entry) => match scan.object(entry.value_start) {
                    // Only our fields change: the user's `env` and others stay.
                    Some(object) => {
                        for (key, value) in json_fields(client, command) {
                            match object.member(key) {
                                Some(member) => edits.push(Edit {
                                    start: member.value_start,
                                    end: member.value_end,
                                    text: value,
                                }),
                                None => {
                                    edits.push(insert_member(&scan, &object, key, &unit, |_| {
                                        value.clone()
                                    }))
                                }
                            }
                        }
                    }
                    None => {
                        let indent = scan.line_indent(entry.key_start).unwrap_or("").to_string();
                        edits.push(Edit {
                            start: entry.value_start,
                            end: entry.value_end,
                            text: json_entry(client, command, &indent, &unit),
                        });
                    }
                },
            }
        }
    }
    // Several insertions at one offset keep their order.
    edits.sort_by_key(|e| std::cmp::Reverse((e.start, e.end)));
    let crlf = body.contains("\r\n");
    let mut out = body.to_string();
    let mut pending: Vec<Edit> = Vec::new();
    for edit in edits {
        if let Some(last) = pending.last_mut()
            && last.start == edit.start
            && last.end == edit.end
            && edit.start == edit.end
        {
            // Same insertion point: in the order they were planned.
            last.text.push_str(&edit.text);
            continue;
        }
        pending.push(edit);
    }
    for edit in pending {
        let text = if crlf {
            edit.text.replace('\n', "\r\n")
        } else {
            edit.text
        };
        out.replace_range(edit.start..edit.end, &text);
    }
    Ok(out)
}

// --- The change, line by line -------------------------------------------------------------

/// The lines that differ between `before` and `after`, with `context` unchanged lines around
/// each change and gaps for the rest. The common start and end are set aside first, so a
/// small change in a large file stays cheap.
pub fn line_diff(before: &str, after: &str, context: usize) -> Vec<DiffLine> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut all: Vec<DiffLine> = a[..prefix]
        .iter()
        .map(|l| DiffLine::Same((*l).into()))
        .collect();
    if am.len().saturating_mul(bm.len()) > 4_000_000 {
        all.extend(am.iter().map(|l| DiffLine::Removed((*l).into())));
        all.extend(bm.iter().map(|l| DiffLine::Added((*l).into())));
    } else {
        // Longest common subsequence of the middle.
        let (n, m) = (am.len(), bm.len());
        let mut table = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                table[i * (m + 1) + j] = if am[i] == bm[j] {
                    table[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    table[(i + 1) * (m + 1) + j].max(table[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                all.push(DiffLine::Same(am[i].into()));
                i += 1;
                j += 1;
            } else if j < m
                // On a tie the old line goes first: `- a` then `+ a,`, as diffs usually read.
                && (i == n || table[i * (m + 1) + j + 1] > table[(i + 1) * (m + 1) + j])
            {
                all.push(DiffLine::Added(bm[j].into()));
                j += 1;
            } else {
                all.push(DiffLine::Removed(am[i].into()));
                i += 1;
            }
        }
    }
    all.extend(
        a[a.len() - suffix..]
            .iter()
            .map(|l| DiffLine::Same((*l).into())),
    );
    // Keep `context` lines around changes.
    let changed: Vec<bool> = all
        .iter()
        .map(|l| !matches!(l, DiffLine::Same(_)))
        .collect();
    let mut keep = vec![false; all.len()];
    for (index, is_changed) in changed.iter().enumerate() {
        if *is_changed {
            let from = index.saturating_sub(context);
            let to = (index + context + 1).min(all.len());
            keep[from..to].iter_mut().for_each(|k| *k = true);
        }
    }
    let mut out = Vec::new();
    let mut skipped = 0;
    for (line, kept) in all.into_iter().zip(keep) {
        if kept {
            if skipped > 0 {
                out.push(DiffLine::Gap(skipped));
                skipped = 0;
            }
            out.push(line);
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push(DiffLine::Gap(skipped));
    }
    out
}

/// The diff as text, for logs and tests: `+`, `-`, two spaces, and `…` for gaps.
pub fn diff_text(lines: &[DiffLine]) -> String {
    let mut out = String::new();
    for line in lines {
        let _ = match line {
            DiffLine::Same(text) => writeln!(out, "  {text}"),
            DiffLine::Added(text) => writeln!(out, "+ {text}"),
            DiffLine::Removed(text) => writeln!(out, "- {text}"),
            DiffLine::Gap(n) => writeln!(out, "… {n} unchanged"),
        };
    }
    out
}

// --- Which `spelaiondb` the clients run ----------------------------------------------------

/// Names the `spelaiondb` binary the clients should run, over every other place.
pub const CLI_ENV: &str = "SPELAION_CLI";

/// Where the `spelaiondb` binary was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliSource {
    /// [`CLI_ENV`].
    Override,
    /// Next to the app's own binary: installed with it, or built in the same `target`.
    Bundled,
    /// On the `PATH` (for example `cargo install`).
    Path,
}

fn cli_file_name(os: Os) -> &'static str {
    match os {
        Os::Windows => "spelaiondb.exe",
        Os::MacOs | Os::Linux => "spelaiondb",
    }
}

/// The `spelaiondb` binary the AI clients should run: [`CLI_ENV`], else the one next to
/// `app_exe` (the installers put it there, ADR-033), else the first on `path`. An AppImage
/// runs from a mount that changes each time, so its neighbour is never used.
pub fn find_cli(
    app_exe: Option<&Path>,
    override_path: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
    appimage: bool,
    os: Os,
) -> Option<(PathBuf, CliSource)> {
    if let Some(chosen) = override_path {
        return chosen
            .is_file()
            .then(|| (chosen.to_path_buf(), CliSource::Override));
    }
    let name = cli_file_name(os);
    if !appimage && let Some(dir) = app_exe.and_then(Path::parent) {
        let next = dir.join(name);
        if next.is_file() {
            return Some((next, CliSource::Bundled));
        }
    }
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_absolute() && candidate.is_file())
        .map(|found| (found, CliSource::Path))
}
