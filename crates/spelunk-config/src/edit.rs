//! Writes configuration from the UI preserving comments, order and formatting, so that
//! changes in git stay readable (ADR-009).

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::ConfigError;
use crate::model::ConnectionFile;

/// JSON Schema URL put in the header of new files (for editors with TOML support such as
/// Taplo/Even Better TOML).
pub const CONNECTION_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/spelunkdb/spelunkdb/main/schema/connection.schema.json";

fn read_doc(path: &Path) -> Result<(String, DocumentMut), ConfigError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::new(path, format!("could not read: {e}")))?;
    let doc = content.parse::<DocumentMut>().map_err(|e| match e.span() {
        Some(span) => ConfigError::at(path, &content, span.start, e.message()),
        None => ConfigError::new(path, e.message()),
    })?;
    Ok((content, doc))
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<(), ConfigError> {
    // Write to a temporary file first and then rename: an interruption never leaves a broken file.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string()).map_err(|e| ConfigError::new(&tmp, e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| ConfigError::new(path, e.to_string()))
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
    write_doc(path, &doc)
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
    write_doc(path, &doc)
}

/// Creates the file of a new connection at `connections_dir/<id>.toml` (with folders if the
/// id has them). Fails if it already exists: it never overwrites a connection.
pub fn create_connection(
    connections_dir: &Path,
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
    std::fs::write(&path, content).map_err(|e| ConfigError::new(&path, e.to_string()))?;
    Ok(path)
}
