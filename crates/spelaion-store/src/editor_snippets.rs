//! The user's own editor snippets and the template of new scripts (SDB-384): personal text
//! kept with the other personal settings, one row per snippet, shared by every workspace of
//! this user directory. A snippet is text the editor inserts when its name is typed before
//! Tab; nothing here is ever run.
//!
//! The store keeps what it is given: what a snippet may hold (bounded sizes, no hidden
//! characters) is checked by the core before it saves and again when it reads
//! (`spelaion-core`'s `editor_snippets.rs`), with the rule a connection's description has.
//! Only the name is checked here, because it is part of the row's key.

use rusqlite::params;

use crate::{Store, StoreError};

/// The key of a snippet's row is this and its name.
const SNIPPET_KEY: &str = "editor.snippet:";
/// The key of the template's row.
const TEMPLATE_KEY: &str = "editor.new_script_template";

/// One of the user's editor snippets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorSnippet {
    /// What is typed before Tab: letters, digits and `_`, starting with a letter or `_`.
    pub name: String,
    /// What it is for, shown next to the name where snippets are offered.
    pub description: String,
    /// The text inserted, with its fields (`${1:name}`) as the built-in snippets write them.
    pub body: String,
}

/// Whether `name` can be a snippet's name: an identifier of ASCII letters, digits and `_`
/// that does not start with a digit, of at most `max` characters.
pub fn is_snippet_name(name: &str, max: usize) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= max
}

/// Longest name a row's key may carry; the core's own limit is lower.
const KEY_NAME_MAX: usize = 64;

impl Store {
    /// Every stored snippet, by name. A row that cannot be read as one is left out.
    pub fn editor_snippets(&self) -> Result<Vec<EditorSnippet>, StoreError> {
        let mut statement = self.conn.prepare(
            "select key, value from personal_settings
             where substr(key, 1, ?1) = ?2 order by key",
        )?;
        let rows = statement.query_map(params![SNIPPET_KEY.len() as i64, SNIPPET_KEY], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut snippets = Vec::new();
        for row in rows {
            let (key, value) = row?;
            let name = &key[SNIPPET_KEY.len()..];
            if !is_snippet_name(name, KEY_NAME_MAX) {
                continue;
            }
            if let Some((description, body)) = decode(&value) {
                snippets.push(EditorSnippet {
                    name: name.to_string(),
                    description,
                    body,
                });
            }
        }
        Ok(snippets)
    }

    /// Saves a snippet, replacing the one of the same name (names differ by case).
    pub fn put_editor_snippet(&self, snippet: &EditorSnippet) -> Result<(), StoreError> {
        if !is_snippet_name(&snippet.name, KEY_NAME_MAX) {
            return Err(StoreError::new("invalid snippet name".into()));
        }
        self.conn.execute(
            "insert into personal_settings (key, value) values (?1, ?2)
             on conflict (key) do update set value = excluded.value",
            params![
                format!("{SNIPPET_KEY}{}", snippet.name),
                encode(&snippet.description, &snippet.body)
            ],
        )?;
        Ok(())
    }

    /// Deletes the snippet of that name; whether there was one.
    pub fn delete_editor_snippet(&self, name: &str) -> Result<bool, StoreError> {
        if !is_snippet_name(name, KEY_NAME_MAX) {
            return Ok(false);
        }
        Ok(self.conn.execute(
            "delete from personal_settings where key = ?1",
            params![format!("{SNIPPET_KEY}{name}")],
        )? > 0)
    }

    /// The text a new script starts with; empty when there is none.
    pub fn new_script_template(&self) -> Result<String, StoreError> {
        Ok(self.personal_setting(TEMPLATE_KEY)?.unwrap_or_default())
    }

    /// Saves the text a new script starts with (empty: none).
    pub fn set_new_script_template(&self, template: &str) -> Result<(), StoreError> {
        self.set_personal_setting(TEMPLATE_KEY, template)
    }
}

/// A snippet's row: the description's length in bytes, a line break, the description and
/// the body. One value, so a snippet is saved or replaced whole.
fn encode(description: &str, body: &str) -> String {
    format!("{}\n{description}{body}", description.len())
}

fn decode(value: &str) -> Option<(String, String)> {
    let (length, rest) = value.split_once('\n')?;
    let length: usize = length.parse().ok()?;
    let description = rest.get(..length)?;
    let body = rest.get(length..)?;
    Some((description.to_string(), body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-editor-snippets-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("state.db")).unwrap(), dir)
    }

    fn snippet(name: &str, description: &str, body: &str) -> EditorSnippet {
        EditorSnippet {
            name: name.into(),
            description: description.into(),
            body: body.into(),
        }
    }

    #[test]
    fn snippets_are_saved_listed_by_name_replaced_and_deleted() {
        let (store, dir) = store();
        assert!(store.editor_snippets().unwrap().is_empty());
        let top = snippet(
            "top",
            "The first rows\nof a table",
            "SELECT ${1:*}\nFROM ${2:table}\nLIMIT ${3:10};${0}",
        );
        let count = snippet("cnt", "", "SELECT count(*) FROM ${1:table};");
        store.put_editor_snippet(&top).unwrap();
        store.put_editor_snippet(&count).unwrap();
        assert_eq!(
            store.editor_snippets().unwrap(),
            [count.clone(), top.clone()]
        );
        // The same name replaces; another case is another snippet.
        let top2 = snippet("top", "Ünïcode ✓", "select 1; -- ñ\n");
        store.put_editor_snippet(&top2).unwrap();
        store.put_editor_snippet(&snippet("Top", "", "x")).unwrap();
        assert_eq!(
            store.editor_snippets().unwrap(),
            [snippet("Top", "", "x"), count.clone(), top2.clone()]
        );
        assert!(store.delete_editor_snippet("Top").unwrap());
        assert!(!store.delete_editor_snippet("Top").unwrap());
        assert!(!store.delete_editor_snippet("missing").unwrap());
        assert_eq!(store.editor_snippets().unwrap(), [count, top2]);
        drop(store);
        // They are still there when the store opens again.
        let store = Store::open(&dir.join("state.db")).unwrap();
        assert_eq!(store.editor_snippets().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_name_is_an_identifier_and_never_reaches_another_setting() {
        let (store, dir) = store();
        store.set_theme("dark").unwrap();
        for name in [
            "",
            "1st",
            "a b",
            "a-b",
            "ñu",
            "a%",
            "x:y",
            "x'; drop table personal_settings; --",
            &"a".repeat(65),
        ] {
            assert!(
                store.put_editor_snippet(&snippet(name, "", "x")).is_err(),
                "{name:?}"
            );
            assert!(!store.delete_editor_snippet(name).unwrap(), "{name:?}");
        }
        assert!(store.editor_snippets().unwrap().is_empty());
        // A snippet's key is its own: neither the template nor another setting is one, and
        // deleting a snippet named like a setting's tail deletes nothing else.
        store.set_new_script_template("-- hello\n").unwrap();
        store
            .put_editor_snippet(&snippet("theme", "", "x"))
            .unwrap();
        assert!(store.delete_editor_snippet("theme").unwrap());
        assert_eq!(store.get_theme().unwrap(), "dark");
        assert_eq!(store.new_script_template().unwrap(), "-- hello\n");
        assert!(store.editor_snippets().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_row_that_is_not_a_snippet_is_left_out() {
        let (store, dir) = store();
        store.put_editor_snippet(&snippet("ok", "d", "b")).unwrap();
        for (key, value) in [
            ("editor.snippet:broken", "no length line"),
            ("editor.snippet:short", "99\nabc"),
            ("editor.snippet:split", "1\nñx"),
            ("editor.snippet:bad name", "0\nbody"),
            ("editor.snippet:", "0\nbody"),
        ] {
            store.set_personal_setting(key, value).unwrap();
        }
        assert_eq!(store.editor_snippets().unwrap(), [snippet("ok", "d", "b")]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_template_is_empty_until_set_and_can_be_emptied() {
        let (store, dir) = store();
        assert_eq!(store.new_script_template().unwrap(), "");
        store
            .set_new_script_template("-- ${1:what}\nSELECT 1;\n")
            .unwrap();
        assert_eq!(
            store.new_script_template().unwrap(),
            "-- ${1:what}\nSELECT 1;\n"
        );
        store.set_new_script_template("").unwrap();
        assert_eq!(store.new_script_template().unwrap(), "");
        let _ = std::fs::remove_dir_all(dir);
    }
}
