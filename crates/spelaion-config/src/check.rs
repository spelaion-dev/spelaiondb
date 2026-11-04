//! `spelaiondb check`: what a team repository's CI and pre-commit hook look at before a change
//! to `.spelaion/` is merged (ADR-022). It reads only the repository, never the user's own
//! configuration, secrets or keychain, so it runs anywhere.
//!
//! Errors always fail the check; warnings fail it only when asked to be strict.

use std::path::{Path, PathBuf};

use crate::load::{Config, ConnectionEntry, Source, TunnelEntry, load, workspace_dir};
use crate::model::{SecretSpec, TlsModeConfig, WritePolicy};
use crate::secrets::SecretRef;
use crate::trust::team_known_hosts;
use crate::vars::valid_var_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: PathBuf,
    pub severity: Severity,
    pub message: String,
}

/// Checks the repository whose root (the folder holding `.spelaion/`) is `root`.
pub fn check_workspace(root: &Path) -> Vec<Finding> {
    // A user directory that does not exist: only the repository's files are loaded.
    let config = load(
        Some(root),
        &workspace_dir(root).join(".spelaion-check-no-user"),
    );
    let mut findings = Vec::new();
    let mut add = |path: &Path, severity, message: String| {
        findings.push(Finding {
            path: path.to_path_buf(),
            severity,
            message,
        })
    };

    // Files that do not load: syntax, unknown fields, missing values, invalid combinations.
    for problem in &config.problems {
        add(&problem.file, Severity::Error, problem.message.clone());
    }
    for entry in config
        .connections
        .iter()
        .filter(|c| c.id.source == Source::Workspace)
    {
        check_connection(&config, root, entry, &mut add);
    }
    for tunnel in config
        .tunnels
        .iter()
        .filter(|t| t.source == Source::Workspace)
    {
        check_tunnel(root, tunnel, &mut add);
    }
    check_known_hosts(root, &mut add);
    check_committed_keys(&workspace_dir(root), &mut add);

    findings.sort_by(|a, b| (&a.path, a.severity).cmp(&(&b.path, b.severity)));
    findings
}

type Add<'a> = dyn FnMut(&Path, Severity, String) + 'a;

fn check_connection(config: &Config, root: &Path, entry: &ConnectionEntry, add: &mut Add<'_>) {
    let path = &entry.file_path;
    let file = &entry.file;
    for text in crate::vars::text_values(file) {
        if let Err(message) = check_variables(&text) {
            add(path, Severity::Error, message);
        }
    }
    if let Some(secret) = &file.password {
        check_secret(path, "password", secret, add);
    }
    if let Some(name) = &file.tunnel
        && config.tunnel_for(Source::Workspace, name).is_none()
    {
        add(
            path,
            Severity::Error,
            format!("tunnel {name:?} does not exist in .spelaion/tunnels/"),
        );
    }
    if let Some(key) = &file.tls.key
        && inside_repository(key, root)
    {
        add(
            path,
            Severity::Error,
            format!(
                "tls.key {key:?} is inside the repository: private keys stay in each person's folder (~/…) or come from a ${{user.…}} variable"
            ),
        );
    }
    let production = file.env.as_deref() == Some("prod");
    if production && file.mcp.exposed && file.mcp.writes == WritePolicy::Allow {
        add(
            path,
            Severity::Warning,
            "a production connection lets AI agents write (mcp.writes = \"allow\")".into(),
        );
    }
    if file.mcp.writes == WritePolicy::Confirm {
        add(
            path,
            Severity::Warning,
            "mcp.writes = \"confirm\" is not supported yet: agents only read".into(),
        );
    }
    let direct = file.tunnel.is_none() && file.kubernetes.is_none();
    let verified = matches!(
        file.tls.mode,
        TlsModeConfig::VerifyCa | TlsModeConfig::VerifyFull
    );
    if production && direct && !file.engine.is_file() && !verified {
        add(
            path,
            Severity::Warning,
            "a production connection does not verify the server's certificate (tls.mode verify-ca or verify-full)".into(),
        );
    }
}

fn check_tunnel(root: &Path, tunnel: &TunnelEntry, add: &mut Add<'_>) {
    let path = &tunnel.file_path;
    for text in crate::vars::text_values(&tunnel.file) {
        if let Err(message) = check_variables(&text) {
            add(path, Severity::Error, message);
        }
    }
    for (i, hop) in tunnel.file.hops.iter().enumerate() {
        if let Some(secret) = &hop.password {
            check_secret(path, &format!("hop {} password", i + 1), secret, add);
        }
        if let Some(key) = &hop.identity_file
            && inside_repository(key, root)
        {
            add(
                path,
                Severity::Warning,
                format!(
                    "hop {}: identity_file {key:?} is inside the repository: a private key in git is shared with everyone who clones it",
                    i + 1
                ),
            );
        }
    }
}

fn check_secret(path: &Path, what: &str, secret: &SecretSpec, add: &mut Add<'_>) {
    match secret.secret.parse::<SecretRef>() {
        Err(message) => add(path, Severity::Error, format!("{what}: {message}")),
        Ok(SecretRef::Command(command)) => {
            if let Some(why) = password_in_command(&command) {
                add(
                    path,
                    Severity::Error,
                    format!(
                        "{what}: the command seems to hold a password ({why}): read it from a secret manager, the keychain or an environment variable instead"
                    ),
                );
            }
        }
        Ok(
            SecretRef::Keychain(_)
            | SecretRef::Env(_)
            | SecretRef::OnePassword(_)
            | SecretRef::Prompt,
        ) => {}
    }
}

/// Why a `cmd:` looks like it has a password written in it, if it does.
fn password_in_command(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    // The specific ones first, so the message names what was found.
    for (needle, why) in [
        ("pgpassword=", "PGPASSWORD="),
        ("mysql_pwd=", "MYSQL_PWD="),
        ("sshpass -p", "sshpass -p"),
        ("--password", "--password"),
        ("password=", "password="),
        ("passwd=", "passwd="),
    ] {
        if lower.contains(needle) {
            return Some(why);
        }
    }
    // `scheme://user:secret@host`
    if let Some(after) = command.split("://").nth(1)
        && let Some(credentials) = after.split(['/', ' ']).next()
        && let Some((user_info, _)) = credentials.rsplit_once('@')
        && user_info.contains(':')
    {
        return Some("a URL with user:password@");
    }
    None
}

/// `${…}` that would not resolve: unclosed, an unknown kind or an invalid name.
fn check_variables(text: &str) -> Result<(), String> {
    let mut rest = text;
    while let Some(pos) = rest.find('$') {
        rest = &rest[pos..];
        if let Some(after) = rest.strip_prefix("$$") {
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let end = after
                .find('}')
                .ok_or_else(|| format!("unclosed variable in {text:?}"))?;
            let name = &after[..end];
            let valid = match (name.strip_prefix("user."), name.strip_prefix("env:")) {
                (Some(user), _) => valid_var_name(user),
                (None, Some(env)) => {
                    !env.is_empty() && env.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                }
                (None, None) => false,
            };
            if !valid {
                return Err(format!(
                    "${{{name}}} is not a variable: use ${{user.<name>}} or ${{env:<NAME>}}"
                ));
            }
            rest = &after[end + 1..];
        } else {
            rest = &rest[1..];
        }
    }
    Ok(())
}

/// A configured path that would point inside the repository: relative paths hang from its
/// root (ADR-009), and an absolute one may be inside it too. `..` is followed, so
/// `../keys/x` is outside.
fn inside_repository(path: &str, root: &Path) -> bool {
    if path.starts_with("~/") || path.starts_with("~\\") || path.starts_with("${") {
        return false;
    }
    let normalize = |path: &Path| {
        let mut out = PathBuf::new();
        for part in path.components() {
            match part {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        out
    };
    normalize(&root.join(path)).starts_with(normalize(root))
}

fn check_known_hosts(root: &Path, add: &mut Add<'_>) {
    let path = team_known_hosts(root);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for (n, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let needed = if fields[0].starts_with('@') { 4 } else { 3 };
        if fields.len() < needed {
            add(
                &path,
                Severity::Warning,
                format!(
                    "line {}: expected \"<hosts> <key type> <key>\", it will be ignored",
                    n + 1
                ),
            );
        }
    }
}

/// Any file under `.spelaion/` holding a private key: committed, it is shared with everyone
/// who clones the repository.
fn check_committed_keys(dir: &Path, add: &mut Add<'_>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            check_committed_keys(&path, add);
        } else if let Ok(content) = std::fs::read(&path)
            && content.len() < 1_000_000
            && String::from_utf8_lossy(&content).contains("PRIVATE KEY-----")
        {
            add(
                &path,
                Severity::Error,
                "this file holds a private key: remove it from the repository (and from its history) and replace the key".into(),
            );
        }
    }
}
