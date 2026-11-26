//! Exporting and importing the user's personal settings (SDB-31): one JSON file that moves how
//! the user likes the app to another computer. It holds no secret and no connection; personal
//! connections move in their own encrypted file (ADR-088).
//!
//! What it carries:
//! - the portable rows of the local store's personal settings (`spelaion-store`'s
//!   `portable.rs`): appearance, editor, grid, display, copy, export, scripts, map, explorer,
//!   keyboard shortcuts, editor snippets and the MCP server's settings; never what names this
//!   computer's folders or programs;
//! - the personal `user.toml` and `connection-types.toml` as their text (`spelaion-config`'s
//!   `settings_files.rs`).
//!
//! An import checks the whole file before it writes anything: the store's rows go into a store
//! in memory first, through the same triggers, and the core checks what they hold (shortcut
//! keys, snippets, the template of new scripts, the name pattern of new connections, the tile
//! server) as it does when they are saved from Settings. Then the personal files are written,
//! then the store's rows in one transaction; when that fails, the files are put back. The rows
//! replace the portable settings here (one the file does not have goes back to its default);
//! the personal files only add (`settings_files.rs`). Nothing loosens a safeguard: the MCP
//! read-only mode stays on, masked columns are only added, and a connection type defined here
//! stays as it is.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spelaion_config::atomic_file::{AtomicWriter, NewFileMode};
use spelaion_config::{PersonalFiles, PersonalFilesPlan};
use spelaion_store::Store;

use crate::history::{HistoryError, QueryHistory};

/// What the file says it is.
const FORMAT: &str = "spelaiondb-settings";
/// The version this build writes and the newest it reads.
pub const SETTINGS_FILE_VERSION: u32 = 1;
/// The largest settings file an import reads.
pub const SETTINGS_FILE_MAX: u64 = 8 * 1024 * 1024;
/// The most rows a settings file may carry.
const MAX_ROWS: usize = 10_000;

const SHORTCUT_PREFIX: &str = "shortcut:";
const SNIPPET_PREFIX: &str = "editor.snippet:";
const USER_FILE: &str = "user.toml";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    format: String,
    version: u32,
    app_version: String,
    exported_at: String,
    settings: BTreeMap<String, String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsTransferError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("this is not a SpelaionDB settings file")]
    NotASettingsFile,
    #[error(
        "this settings file is from a newer version of SpelaionDB (format {0}; this one reads up to {SETTINGS_FILE_VERSION})"
    )]
    Newer(u32),
    #[error("the settings file is larger than {} MiB", SETTINGS_FILE_MAX / 1024 / 1024)]
    TooLarge,
    /// A setting in the file that this version would refuse from Settings: nothing changed.
    #[error("the setting {setting} cannot be used: {reason}")]
    Refused { setting: String, reason: String },
    #[error("{0}")]
    Config(#[from] spelaion_config::ConfigError),
    #[error("{0}")]
    Store(#[from] spelaion_store::StoreError),
    #[error("{0}")]
    History(#[from] HistoryError),
}

/// What a settings file holds, and what importing it changes or changed here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsSummary {
    /// The version of SpelaionDB that exported it, and when (UTC, ISO 8601).
    pub app_version: String,
    pub exported_at: String,
    /// Settings other than shortcuts and snippets.
    pub settings: usize,
    /// Commands whose keys the user changed.
    pub shortcuts: usize,
    /// The user's own editor snippets.
    pub snippets: usize,
    /// `${user.…}` variables that are new or get another value here.
    pub variables: Vec<String>,
    /// The tunnels' port range it sets, when it changes.
    pub tunnel_ports: Option<(u16, u16)>,
    /// Masked columns it adds.
    pub masking_added: Vec<String>,
    /// Connection types it adds, and those it leaves as this computer defines them.
    pub types_added: Vec<String>,
    pub types_kept: Vec<String>,
    /// The MCP read-only mode is on here and stays on, though the file has it off.
    pub mcp_read_only_kept: bool,
    /// Settings of the file this version does not know (from a newer version), left out.
    pub skipped: Vec<String>,
}

fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    crate::audit::iso_time(ms)
}

fn count(settings: &BTreeMap<String, String>, summary: &mut SettingsSummary) {
    for key in settings.keys() {
        if key.starts_with(SHORTCUT_PREFIX) {
            summary.shortcuts += 1;
        } else if key.starts_with(SNIPPET_PREFIX) {
            summary.snippets += 1;
        } else {
            summary.settings += 1;
        }
    }
}

/// Writes the user's personal settings to `path` (replacing it whole, or not at all).
pub async fn export_settings(
    history: &QueryHistory,
    user_dir: &Path,
    path: &Path,
    app_version: &str,
) -> Result<SettingsSummary, SettingsTransferError> {
    let settings: BTreeMap<String, String> =
        history.portable_settings().await?.into_iter().collect();
    let personal = spelaion_config::read_personal_files(user_dir)?;
    let mut files = BTreeMap::new();
    if let Some(text) = personal.user {
        files.insert(USER_FILE.to_string(), text);
    }
    if let Some(text) = personal.connection_types {
        files.insert(spelaion_config::CONNECTION_TYPES_FILE.to_string(), text);
    }
    let file = SettingsFile {
        format: FORMAT.into(),
        version: SETTINGS_FILE_VERSION,
        app_version: app_version.into(),
        exported_at: now_iso(),
        settings,
        files,
    };
    let text = serde_json::to_vec_pretty(&file).map_err(|e| SettingsTransferError::Write {
        path: path.to_path_buf(),
        source: std::io::Error::other(e),
    })?;
    let write = |source| SettingsTransferError::Write {
        path: path.to_path_buf(),
        source,
    };
    let mut writer = AtomicWriter::create(path, NewFileMode::OwnerOnly).map_err(write)?;
    writer.write_all(&text).map_err(write)?;
    writer.commit().map_err(write)?;
    let mut summary = SettingsSummary {
        app_version: file.app_version,
        exported_at: file.exported_at,
        ..Default::default()
    };
    count(&file.settings, &mut summary);
    Ok(summary)
}

/// A settings file read and checked whole, ready to import.
struct Checked {
    rows: Vec<(String, String)>,
    plan: PersonalFilesPlan,
    summary: SettingsSummary,
}

fn read_file(path: &Path) -> Result<SettingsFile, SettingsTransferError> {
    let read = |source| SettingsTransferError::Read {
        path: path.to_path_buf(),
        source,
    };
    let size = std::fs::metadata(path).map_err(read)?.len();
    if size > SETTINGS_FILE_MAX {
        return Err(SettingsTransferError::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(read)?;
    parse_file(&bytes)
}

fn parse_file(bytes: &[u8]) -> Result<SettingsFile, SettingsTransferError> {
    // The format and version first, so a newer file says so instead of failing on a field
    // this version does not know.
    #[derive(Deserialize)]
    struct Head {
        format: Option<String>,
        version: Option<u32>,
    }
    let head: Head =
        serde_json::from_slice(bytes).map_err(|_| SettingsTransferError::NotASettingsFile)?;
    match (head.format.as_deref(), head.version) {
        (Some(FORMAT), Some(version)) if version > SETTINGS_FILE_VERSION => {
            Err(SettingsTransferError::Newer(version))
        }
        (Some(FORMAT), Some(_)) => {
            serde_json::from_slice(bytes).map_err(|_| SettingsTransferError::NotASettingsFile)
        }
        _ => Err(SettingsTransferError::NotASettingsFile),
    }
}

fn refused(setting: &str, reason: impl ToString) -> SettingsTransferError {
    SettingsTransferError::Refused {
        setting: setting.to_string(),
        reason: reason.to_string(),
    }
}

/// Checks the store's rows as Settings would: in a store in memory (its triggers), then with
/// the core's checks of what they hold.
fn check_rows(rows: &[(String, String)]) -> Result<(), SettingsTransferError> {
    if rows.len() > MAX_ROWS {
        return Err(SettingsTransferError::TooLarge);
    }
    let scratch = Store::open_in_memory()?;
    scratch
        .replace_portable_settings(rows)
        .map_err(|e| refused("in the file", e))?;
    let shortcut_rows = rows
        .iter()
        .filter(|(k, _)| k.starts_with(SHORTCUT_PREFIX))
        .count();
    let shortcuts = scratch.shortcut_overrides()?;
    if shortcuts.len() != shortcut_rows {
        return Err(refused(
            "keyboard shortcuts",
            "a shortcut names no command this version knows how to read",
        ));
    }
    for change in &shortcuts {
        if let Some(keys) = &change.keys {
            crate::shortcuts::check_shortcut(keys)
                .map_err(|e| refused(&format!("{SHORTCUT_PREFIX}{}", change.command), e))?;
        }
    }
    if shortcuts.len() > crate::shortcuts::MAX_SHORTCUT_OVERRIDES {
        return Err(refused(
            "keyboard shortcuts",
            crate::shortcuts::ShortcutError::TooMany,
        ));
    }
    let snippet_rows = rows
        .iter()
        .filter(|(k, _)| k.starts_with(SNIPPET_PREFIX))
        .count();
    let snippets = scratch.editor_snippets()?;
    if snippets.len() != snippet_rows {
        return Err(refused("editor snippets", "a snippet cannot be read"));
    }
    for snippet in &snippets {
        crate::editor_snippets::check_snippet(snippet)
            .map_err(|e| refused(&format!("{SNIPPET_PREFIX}{}", snippet.name), e))?;
    }
    if snippets.len() > crate::editor_snippets::MAX_SNIPPETS {
        return Err(refused(
            "editor snippets",
            crate::editor_snippets::SnippetError::TooMany,
        ));
    }
    crate::editor_snippets::check_script_template(&scratch.new_script_template()?)
        .map_err(|e| refused("editor.new_script_template", e))?;
    if let Some(pattern) = scratch.get_connection_name_pattern()? {
        spelaion_config::NamePattern::parse(&pattern)
            .map_err(|e| refused("connection_name_pattern", e))?;
    }
    // The map's reader drops a server it cannot use, so the rows themselves are checked.
    let raw = |key: &str| {
        rows.iter()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str())
    };
    if !raw("map.tile_url").is_empty() {
        spelaion_store::check_tile_server(
            raw("map.tile_name"),
            raw("map.tile_url"),
            raw("map.tile_attribution"),
        )
        .map_err(|e| refused("map.tile_url", e))?;
    }
    spelaion_store::check_attribution_link(raw("map.tile_attribution_url"))
        .map_err(|e| refused("map.tile_attribution_url", e))?;
    Ok(())
}

async fn check(
    history: &QueryHistory,
    user_dir: &Path,
    file: SettingsFile,
) -> Result<Checked, SettingsTransferError> {
    let mut summary = SettingsSummary {
        app_version: file.app_version,
        exported_at: file.exported_at,
        ..Default::default()
    };
    let mut rows = Vec::new();
    let mut known = BTreeMap::new();
    for (key, value) in file.settings {
        if spelaion_store::is_portable_setting(&key) {
            rows.push((key.clone(), value.clone()));
            known.insert(key, value);
        } else {
            summary.skipped.push(key);
        }
    }
    count(&known, &mut summary);
    check_rows(&rows)?;
    let mut personal = PersonalFiles::default();
    for (name, text) in file.files {
        if name == USER_FILE {
            personal.user = Some(text);
        } else if name == spelaion_config::CONNECTION_TYPES_FILE {
            personal.connection_types = Some(text);
        } else {
            summary.skipped.push(name);
        }
    }
    let plan = spelaion_config::plan_personal_files(user_dir, &personal)?;
    summary.variables = plan.vars.keys().cloned().collect();
    summary.tunnel_ports = plan.tunnel_ports;
    summary.masking_added = plan.masking_added.clone();
    summary.types_added = plan.types_added.iter().map(|(id, _)| id.clone()).collect();
    summary.types_kept = plan.types_kept.clone();
    let (read_only_here, _) = history.mcp_settings().await?;
    summary.mcp_read_only_kept = read_only_here
        && !rows
            .iter()
            .any(|(key, value)| key == "mcp.read_only" && value != "off");
    Ok(Checked {
        rows,
        plan,
        summary,
    })
}

/// Reads and checks a settings file, and says what importing it would change. Nothing is
/// written.
pub async fn preview_settings(
    history: &QueryHistory,
    user_dir: &Path,
    path: &Path,
) -> Result<SettingsSummary, SettingsTransferError> {
    let file = read_file(path)?;
    Ok(check(history, user_dir, file).await?.summary)
}

/// Imports a settings file: checked whole first, then the personal files, then the store's
/// rows in one transaction. When the rows fail, the personal files are put back, so nothing
/// changes.
pub async fn import_settings(
    history: &QueryHistory,
    user_dir: &Path,
    path: &Path,
) -> Result<SettingsSummary, SettingsTransferError> {
    let file = read_file(path)?;
    let checked = check(history, user_dir, file).await?;
    let before = spelaion_config::apply_personal_files(user_dir, &checked.plan)?;
    match history.replace_portable_settings(checked.rows).await {
        Ok(done) => {
            let mut summary = checked.summary;
            summary.mcp_read_only_kept = done.kept_mcp_read_only;
            Ok(summary)
        }
        Err(e) => {
            let _ = before.restore();
            Err(e.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(settings: &[(&str, &str)]) -> Vec<u8> {
        serde_json::to_vec(&SettingsFile {
            format: FORMAT.into(),
            version: SETTINGS_FILE_VERSION,
            app_version: "0.18.0".into(),
            exported_at: "2026-11-26T00:00:00Z".into(),
            settings: settings
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            files: BTreeMap::new(),
        })
        .unwrap()
    }

    fn rows(settings: &[(&str, &str)]) -> Vec<(String, String)> {
        settings
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_a_settings_file_of_this_version_or_older_is_read() {
        assert!(parse_file(&file(&[("theme", "dark")])).is_ok());
        assert!(matches!(
            parse_file(b"{\"format\":\"spelaiondb-settings\",\"version\":2,\"later\":true}"),
            Err(SettingsTransferError::Newer(2))
        ));
        for bad in [
            &b"not json"[..],
            b"{}",
            b"{\"format\":\"other\",\"version\":1}",
            b"{\"format\":\"spelaiondb-settings\",\"version\":1}",
            b"{\"format\":\"spelaiondb-settings\",\"version\":1,\"app_version\":\"x\",\"exported_at\":\"x\",\"settings\":{},\"unknown\":1}",
        ] {
            assert!(
                matches!(parse_file(bad), Err(SettingsTransferError::NotASettingsFile)),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn the_core_refuses_what_settings_would_refuse() {
        assert!(
            check_rows(&rows(&[
                ("theme", "dark"),
                ("shortcut:editor.run", "Ctrl+R"),
                ("shortcut:tab.new", ""),
                ("connection_name_pattern", "{engine} {host}"),
            ]))
            .is_ok()
        );
        for bad in [
            rows(&[("theme", "blue")]),
            rows(&[("shortcut:editor.run", "A")]),
            rows(&[("shortcut:editor.run", "Alt+F4")]),
            rows(&[("shortcut:bad id!", "Ctrl+R")]),
            rows(&[("editor.snippet:sel", "not an encoded snippet")]),
            rows(&[("editor.new_script_template", "a\u{202e}b")]),
            rows(&[("connection_name_pattern", "{nonsense}")]),
            rows(&[
                ("map.tile_name", "Tiles"),
                ("map.tile_url", "http://tiles.example/{z}/{x}/{y}.png"),
            ]),
        ] {
            assert!(
                matches!(check_rows(&bad), Err(SettingsTransferError::Refused { .. })),
                "{bad:?}"
            );
        }
    }
}
