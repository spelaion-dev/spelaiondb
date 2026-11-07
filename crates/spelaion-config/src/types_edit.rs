//! Writes the user's `connection-types.toml` from the Settings page (ADR-029), touching only the
//! type being saved: comments and the other types stay.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table};

use crate::edit::{read_doc, write_doc};
use crate::error::ConfigError;
use crate::load::{CONNECTION_TYPES_FILE, validate_type, validate_types};
use crate::model::{ConnectionTypeConfig, ConnectionTypesFile, SCHEMA_VERSION};

/// Saves one personal connection type, creating the file if needed. Empty texts and a zero
/// idle time are left out, as if not set.
pub fn set_personal_type(
    user_dir: &Path,
    id: &str,
    kind: &ConnectionTypeConfig,
) -> Result<(), ConfigError> {
    let path = user_dir.join(CONNECTION_TYPES_FILE);
    let kind = tidy(kind);
    validate_type(id, &kind).map_err(|m| ConfigError::new(&path, m))?;
    let mut doc = if path.is_file() {
        read_doc(&path)?.1
    } else {
        let mut doc = DocumentMut::new();
        doc["schema_version"] = toml_edit::value(i64::from(SCHEMA_VERSION));
        doc
    };
    if !doc.contains_table("types") {
        let mut types = Table::new();
        types.set_implicit(true);
        doc["types"] = Item::Table(types);
    }
    let types = doc["types"]
        .as_table_mut()
        .ok_or_else(|| ConfigError::new(&path, "[types] is not a table"))?;
    let table = match types.get_mut(id).and_then(Item::as_table_mut) {
        Some(table) => table,
        None => {
            types.insert(id, Item::Table(Table::new()));
            types[id]
                .as_table_mut()
                .expect("the table was just inserted")
        }
    };
    let text = |table: &mut Table, key: &str, value: &Option<String>| match value {
        Some(value) => table[key] = toml_edit::value(value.as_str()),
        None => {
            table.remove(key);
        }
    };
    let flag = |table: &mut Table, key: &str, value: bool| {
        if value {
            table[key] = toml_edit::value(true);
        } else {
            table.remove(key);
        }
    };
    text(table, "name", &kind.name);
    match kind.tone {
        Some(tone) => table["tone"] = toml_edit::value(tone.as_str()),
        None => {
            table.remove("tone");
        }
    }
    text(table, "description", &kind.description);
    flag(table, "confirm_execution", kind.confirm_execution);
    flag(table, "confirm_data_changes", kind.confirm_data_changes);
    match kind.close_idle_after {
        Some(seconds) => table["close_idle_after"] = toml_edit::value(i64::from(seconds)),
        None => {
            table.remove("close_idle_after");
        }
    }
    save(&path, &doc)
}

/// Removes one personal connection type (or the personal changes to a built-in or team type).
pub fn remove_personal_type(user_dir: &Path, id: &str) -> Result<(), ConfigError> {
    let path = user_dir.join(CONNECTION_TYPES_FILE);
    if !path.is_file() {
        return Ok(());
    }
    let (_, mut doc) = read_doc(&path)?;
    if let Some(types) = doc.get_mut("types").and_then(Item::as_table_mut) {
        types.remove(id);
    }
    save(&path, &doc)
}

fn tidy(kind: &ConnectionTypeConfig) -> ConnectionTypeConfig {
    let text = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    ConnectionTypeConfig {
        name: text(&kind.name),
        tone: kind.tone,
        description: text(&kind.description),
        confirm_execution: kind.confirm_execution,
        confirm_data_changes: kind.confirm_data_changes,
        close_idle_after: kind.close_idle_after.filter(|s| *s > 0),
    }
}

/// The edited file must still load before it replaces the old one.
fn save(path: &Path, doc: &DocumentMut) -> Result<(), ConfigError> {
    let edited = doc.to_string();
    let file: ConnectionTypesFile = toml::from_str(&edited)
        .map_err(|e| ConfigError::new(path, format!("the edited file is not valid: {e}")))?;
    validate_types(path, file)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::new(parent, e.to_string()))?;
    }
    write_doc(path, doc)
}
