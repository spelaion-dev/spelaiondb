//! Writes configuration from the UI preserving comments, order and formatting, so that
//! changes in git stay readable (ADR-009).

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::atomic_file::{self, NewFileMode};
use crate::error::ConfigError;
use crate::load::Source;
use crate::model::ConnectionFile;

pub(crate) fn revision(path: &Path, value: &impl serde::Serialize) -> Result<String, ConfigError> {
    serde_json::to_string(&(path, value)).map_err(|e| ConfigError::new(path, e.to_string()))
}

/// JSON Schema URL put in the header of new files (for editors with TOML support such as
/// Taplo/Even Better TOML).
pub const CONNECTION_SCHEMA_URL: &str = "https://spelaion.dev/schema/db/connection.schema.json";

pub(crate) fn read_doc(path: &Path) -> Result<(String, DocumentMut), ConfigError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::new(path, format!("could not read: {e}")))?;
    let doc = content.parse::<DocumentMut>().map_err(|e| match e.span() {
        Some(span) => ConfigError::at(path, &content, span.start, e.message()),
        None => ConfigError::new(path, e.message()),
    })?;
    Ok((content, doc))
}

/// Defines `${user.<name>}` variables in the user's `user.toml` (ADR-022), creating it if
/// needed. Only the `[vars]` entries given are touched: comments and everything else stay.
pub fn set_user_vars(
    user_dir: &Path,
    values: &std::collections::BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    let path = user_dir.join("user.toml");
    for (name, value) in values {
        if !crate::vars::valid_var_name(name) {
            return Err(ConfigError::new(
                &path,
                format!(
                    "{name:?} is not a valid variable name (letters, digits, '_', '-' and '.')"
                ),
            ));
        }
        if value.trim().is_empty() {
            return Err(ConfigError::new(
                &path,
                format!("the value of ${{user.{name}}} cannot be empty"),
            ));
        }
    }
    let mut doc = if path.is_file() {
        std::fs::read_to_string(&path)
            .map_err(|e| ConfigError::new(&path, e.to_string()))?
            .parse::<DocumentMut>()
            .map_err(|e| ConfigError::new(&path, e.to_string()))?
    } else {
        let mut doc = DocumentMut::new();
        doc["schema_version"] = toml_edit::value(i64::from(crate::model::SCHEMA_VERSION));
        doc
    };
    if !doc.contains_table("vars") {
        doc["vars"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let vars = doc["vars"]
        .as_table_mut()
        .ok_or_else(|| ConfigError::new(&path, "[vars] is not a table"))?;
    for (name, value) in values {
        vars[name.as_str()] = toml_edit::value(value.trim());
    }
    // The result must still load before it replaces the file.
    let edited = doc.to_string();
    toml::from_str::<crate::model::UserFile>(&edited)
        .map_err(|e| ConfigError::new(&path, format!("the edited file is not valid: {e}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::new(parent, e.to_string()))?;
    }
    write_doc(&path, &doc, NewFileMode::OwnerOnly)
}

/// Sets (or, with `None`, removes) the personal range of local ports for tunnels in
/// `user.toml` (`[tunnels]`); the rest of the file stays as it was.
pub fn set_tunnel_ports(user_dir: &Path, range: Option<(u16, u16)>) -> Result<(), ConfigError> {
    let path = user_dir.join("user.toml");
    let preferences = crate::model::TunnelPreferences {
        port_min: range.map(|(min, _)| min),
        port_max: range.map(|(_, max)| max),
    };
    preferences
        .range()
        .map_err(|message| ConfigError::new(&path, message))?;
    let mut doc = if path.is_file() {
        std::fs::read_to_string(&path)
            .map_err(|e| ConfigError::new(&path, e.to_string()))?
            .parse::<DocumentMut>()
            .map_err(|e| ConfigError::new(&path, e.to_string()))?
    } else {
        let mut doc = DocumentMut::new();
        doc["schema_version"] = toml_edit::value(i64::from(crate::model::SCHEMA_VERSION));
        doc
    };
    match range {
        None => {
            doc.as_table_mut().remove("tunnels");
        }
        Some((min, max)) => {
            if !doc.contains_table("tunnels") {
                doc["tunnels"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let tunnels = doc["tunnels"]
                .as_table_mut()
                .ok_or_else(|| ConfigError::new(&path, "[tunnels] is not a table"))?;
            tunnels["port_min"] = toml_edit::value(i64::from(min));
            tunnels["port_max"] = toml_edit::value(i64::from(max));
        }
    }
    let edited = doc.to_string();
    toml::from_str::<crate::model::UserFile>(&edited)
        .map_err(|e| ConfigError::new(&path, format!("the edited file is not valid: {e}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::new(parent, e.to_string()))?;
    }
    write_doc(&path, &doc, NewFileMode::OwnerOnly)
}

/// Replaces `path` with `doc` through [`atomic_file::replace`]: an interruption never leaves a
/// broken file, two writes never share a temporary file, the file keeps its mode and a
/// symbolic link stays a link (its target gets the edit). A file that did not exist gets
/// `new_file`'s mode.
pub(crate) fn write_doc(
    path: &Path,
    doc: &DocumentMut,
    new_file: NewFileMode,
) -> Result<(), ConfigError> {
    atomic_file::replace(path, doc.to_string().as_bytes(), new_file)
        .map_err(|e| ConfigError::new(path, format!("could not write: {e}")))
}

/// The mode of a new file of `source`: the personal folder's are owner-only like the folder
/// itself (`load::ensure_private_dir`); the repository's are ordinary files the team commits.
pub(crate) fn new_file_mode(source: Source) -> NewFileMode {
    match source {
        Source::User => NewFileMode::OwnerOnly,
        Source::Workspace => NewFileMode::Default,
    }
}

/// Changes (or creates) a value, e.g. `set_value(f, &["tls", "mode"], "verify-full")`.
/// The rest of the file stays intact, comments included.
pub fn set_value(path: &Path, key: &[&str], value: impl Into<Value>) -> Result<(), ConfigError> {
    let (last, parents) = key
        .split_last()
        .ok_or_else(|| ConfigError::new(path, "empty key"))?;
    let (_, mut doc) = read_doc(path)?;
    let mut table: &mut Table = doc.as_table_mut();
    for part in parents {
        let item = table
            .entry(part)
            .or_insert_with(|| Item::Table(Table::new()));
        table = item
            .as_table_mut()
            .ok_or_else(|| ConfigError::new(path, format!("{part} is not a table")))?;
    }
    match table.get_mut(last).and_then(Item::as_value_mut) {
        // Keep the comment on the previous value's line.
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = value.into();
            *existing.decor_mut() = decor;
        }
        None => {
            table.insert(last, Item::Value(value.into()));
        }
    }
    write_doc(path, &doc, NewFileMode::Default)
}

/// Removes a value if it exists.
pub fn remove_value(path: &Path, key: &[&str]) -> Result<(), ConfigError> {
    let (last, parents) = key
        .split_last()
        .ok_or_else(|| ConfigError::new(path, "empty key"))?;
    let (_, mut doc) = read_doc(path)?;
    let mut table: &mut Table = doc.as_table_mut();
    for part in parents {
        match table.get_mut(part).and_then(Item::as_table_mut) {
            Some(t) => table = t,
            None => return Ok(()),
        }
    }
    table.remove(last);
    write_doc(path, &doc, NewFileMode::Default)
}

/// Creates the file of a new connection at `connections_dir/<id>.toml` (with folders if the
/// id has them), the folder of `source`'s connections (which decides the new file's mode on
/// Unix). Fails if it already exists: it never overwrites a connection.
pub fn create_connection(
    connections_dir: &Path,
    source: Source,
    id: &str,
    file: &ConnectionFile,
) -> Result<PathBuf, ConfigError> {
    if id.is_empty() || id.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return Err(ConfigError::new(
            connections_dir,
            format!("invalid identifier: {id:?}"),
        ));
    }
    let path = connections_dir.join(format!("{id}.toml"));
    if path.exists() {
        return Err(ConfigError::new(
            &path,
            "a connection with that identifier already exists",
        ));
    }
    let body = toml::to_string_pretty(file).map_err(|e| ConfigError::new(&path, e.to_string()))?;
    let content = format!("#:schema {CONNECTION_SCHEMA_URL}\n\n{body}");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::new(parent, e.to_string()))?;
    }
    atomic_file::create_new(&path, content.as_bytes(), new_file_mode(source)).map_err(|e| {
        let message = match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                "a connection with that identifier already exists".to_string()
            }
            _ => format!("could not write: {e}"),
        };
        ConfigError::new(&path, message)
    })?;
    Ok(path)
}
