//! The rules of the user's own editor snippets and of the template of new scripts (SDB-384).
//! Both are personal text the editor inserts and never runs. What they may hold is decided
//! here, not in the page: bounded sizes, and plain text without hidden characters, by the
//! same rule a connection's description has (`spelaion_config::check_plain_text`). The same
//! check runs when they are read, so a store written by another program cannot hand the
//! editor what it would have refused to save.

use spelaion_config::{PlainTextError, check_plain_text};
use spelaion_store::{EditorSnippet, Store, StoreError, is_snippet_name};

/// Longest snippet name, in characters: what is typed before Tab.
pub const MAX_SNIPPET_NAME: usize = 32;
/// Longest snippet description, in characters: a line or two next to the name.
pub const MAX_SNIPPET_DESCRIPTION: usize = 200;
/// Longest snippet text, in characters.
pub const MAX_SNIPPET_BODY: usize = 20_000;
/// Most snippets a user may keep.
pub const MAX_SNIPPETS: usize = 200;
/// Longest template of a new script, in characters.
pub const MAX_SCRIPT_TEMPLATE: usize = 20_000;

/// Why a snippet or a template is refused.
#[derive(Debug, thiserror::Error)]
pub enum SnippetError {
    #[error(
        "a snippet's name is 1 to {MAX_SNIPPET_NAME} letters, digits or underscores, and does not start with a digit"
    )]
    Name,
    #[error("a snippet's description is at most {MAX_SNIPPET_DESCRIPTION} characters")]
    DescriptionTooLong,
    #[error("a snippet's description can only hold text, line breaks and tabs")]
    DescriptionHidden,
    #[error("a snippet needs some text")]
    EmptyBody,
    #[error("a snippet's text is at most {MAX_SNIPPET_BODY} characters")]
    BodyTooLong,
    #[error("a snippet's text can only hold text, line breaks and tabs")]
    BodyHidden,
    #[error("there are already {MAX_SNIPPETS} snippets: delete one first")]
    TooMany,
    #[error("the template of a new script is at most {MAX_SCRIPT_TEMPLATE} characters")]
    TemplateTooLong,
    #[error("the template of a new script can only hold text, line breaks and tabs")]
    TemplateHidden,
    #[error("{0}")]
    Store(#[from] StoreError),
}

/// Whether a snippet may be kept and offered.
pub fn check_snippet(snippet: &EditorSnippet) -> Result<(), SnippetError> {
    if !is_snippet_name(&snippet.name, MAX_SNIPPET_NAME) {
        return Err(SnippetError::Name);
    }
    check_plain_text(&snippet.description, MAX_SNIPPET_DESCRIPTION).map_err(|e| match e {
        PlainTextError::TooLong => SnippetError::DescriptionTooLong,
        PlainTextError::Hidden => SnippetError::DescriptionHidden,
    })?;
    if snippet.body.trim().is_empty() {
        return Err(SnippetError::EmptyBody);
    }
    check_plain_text(&snippet.body, MAX_SNIPPET_BODY).map_err(|e| match e {
        PlainTextError::TooLong => SnippetError::BodyTooLong,
        PlainTextError::Hidden => SnippetError::BodyHidden,
    })
}

/// Whether a text may be the template of new scripts (empty: no template).
pub fn check_script_template(template: &str) -> Result<(), SnippetError> {
    check_plain_text(template, MAX_SCRIPT_TEMPLATE).map_err(|e| match e {
        PlainTextError::TooLong => SnippetError::TemplateTooLong,
        PlainTextError::Hidden => SnippetError::TemplateHidden,
    })
}

/// The user's snippets, by name: only those that pass [`check_snippet`], and at most
/// [`MAX_SNIPPETS`] of them.
pub fn editor_snippets(store: &Store) -> Result<Vec<EditorSnippet>, StoreError> {
    Ok(store
        .editor_snippets()?
        .into_iter()
        .filter(|snippet| check_snippet(snippet).is_ok())
        .take(MAX_SNIPPETS)
        .collect())
}

/// Saves a snippet after checking it, replacing the one of the same name. A new name is
/// refused once there are [`MAX_SNIPPETS`].
pub fn save_editor_snippet(store: &Store, snippet: &EditorSnippet) -> Result<(), SnippetError> {
    check_snippet(snippet)?;
    let stored = store.editor_snippets()?;
    if stored.len() >= MAX_SNIPPETS && !stored.iter().any(|s| s.name == snippet.name) {
        return Err(SnippetError::TooMany);
    }
    Ok(store.put_editor_snippet(snippet)?)
}

/// The template of new scripts; one that no longer passes the check reads as none.
pub fn new_script_template(store: &Store) -> Result<String, StoreError> {
    let template = store.new_script_template()?;
    Ok(if check_script_template(&template).is_ok() {
        template
    } else {
        String::new()
    })
}

/// Saves the template of new scripts after checking it (empty: none).
pub fn set_new_script_template(store: &Store, template: &str) -> Result<(), SnippetError> {
    check_script_template(template)?;
    Ok(store.set_new_script_template(template)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-core-snippets-{name}-{}",
            std::process::id()
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
    fn what_should_be_refused_is_refused_and_nothing_of_it_is_stored() {
        let (store, dir) = store("refused");
        let good = snippet("top", "First rows", "SELECT ${1:*}\n\tFROM ${2:table};${0}");
        save_editor_snippet(&store, &good).unwrap();
        let long_name = "a".repeat(MAX_SNIPPET_NAME + 1);
        let long_description = "d".repeat(MAX_SNIPPET_DESCRIPTION + 1);
        let long_body = "b".repeat(MAX_SNIPPET_BODY + 1);
        let refused: Vec<(EditorSnippet, &str)> = vec![
            (snippet("", "", "x"), "name"),
            (snippet("2nd", "", "x"), "name"),
            (snippet("with space", "", "x"), "name"),
            (snippet("añade", "", "x"), "name"),
            (snippet(&long_name, "", "x"), "name"),
            (snippet("a", &long_description, "x"), "description"),
            // A terminal escape, a NUL, a right-to-left override, a zero-width space, a BOM.
            (snippet("a", "say \u{1b}[2J", "x"), "description"),
            (snippet("a", "nul\0", "x"), "description"),
            (snippet("a", "abc\u{202e}cba", "x"), "description"),
            (snippet("a", "", ""), "needs some text"),
            (snippet("a", "", " \n\t"), "needs some text"),
            (snippet("a", "", &long_body), "text is at most"),
            (snippet("a", "", "select 1;\u{200b}"), "text can only hold"),
            (snippet("a", "", "drop\u{202e} table"), "text can only hold"),
            (snippet("a", "", "x\u{feff}"), "text can only hold"),
            (snippet("a", "", "bell\u{7}"), "text can only hold"),
        ];
        for (bad, why) in refused {
            let error = save_editor_snippet(&store, &bad)
                .expect_err(why)
                .to_string();
            assert!(error.contains(why), "{error}");
        }
        // Exactly at the limits is fine, with line breaks and tabs in both texts.
        let edge = snippet(
            &"n".repeat(MAX_SNIPPET_NAME),
            &format!("{}\n\t", "d".repeat(MAX_SNIPPET_DESCRIPTION - 2)),
            &"ñ".repeat(MAX_SNIPPET_BODY),
        );
        save_editor_snippet(&store, &edge).unwrap();
        assert_eq!(editor_snippets(&store).unwrap(), [edge, good]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_snippet_written_around_the_check_is_never_offered() {
        let (store, dir) = store("around");
        save_editor_snippet(&store, &snippet("ok", "", "select 1")).unwrap();
        // The store keeps what it is given: another program could write these.
        store
            .put_editor_snippet(&snippet("hidden", "", "select 1;\u{202e}"))
            .unwrap();
        store
            .put_editor_snippet(&snippet("blank", "", "  "))
            .unwrap();
        store
            .put_editor_snippet(&snippet(&"n".repeat(MAX_SNIPPET_NAME + 1), "", "x"))
            .unwrap();
        store.set_new_script_template("-- \u{1b}[31m").unwrap();
        assert_eq!(
            editor_snippets(&store).unwrap(),
            [snippet("ok", "", "select 1")]
        );
        assert_eq!(new_script_template(&store).unwrap(), "");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn at_most_the_limit_of_snippets_and_one_of_them_can_still_change() {
        let (store, dir) = store("limit");
        for i in 0..MAX_SNIPPETS {
            save_editor_snippet(&store, &snippet(&format!("s{i}"), "", "x")).unwrap();
        }
        assert!(matches!(
            save_editor_snippet(&store, &snippet("one_more", "", "x")),
            Err(SnippetError::TooMany)
        ));
        save_editor_snippet(&store, &snippet("s7", "changed", "y")).unwrap();
        let all = editor_snippets(&store).unwrap();
        assert_eq!(all.len(), MAX_SNIPPETS);
        assert!(all.iter().any(|s| s.name == "s7" && s.body == "y"));
        assert!(store.delete_editor_snippet("s0").unwrap());
        save_editor_snippet(&store, &snippet("one_more", "", "x")).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_template_is_checked_like_a_snippets_text() {
        let (store, dir) = store("template");
        assert_eq!(new_script_template(&store).unwrap(), "");
        set_new_script_template(&store, "-- ${1:purpose}\n\nSELECT 1;\n").unwrap();
        assert_eq!(
            new_script_template(&store).unwrap(),
            "-- ${1:purpose}\n\nSELECT 1;\n"
        );
        for bad in [
            "x".repeat(MAX_SCRIPT_TEMPLATE + 1),
            "select 1;\u{200b}".to_string(),
            "a\u{202e}b".to_string(),
            "nul\0".to_string(),
        ] {
            assert!(set_new_script_template(&store, &bad).is_err());
        }
        // A refused template leaves the one in force; an empty one removes it.
        assert_eq!(
            new_script_template(&store).unwrap(),
            "-- ${1:purpose}\n\nSELECT 1;\n"
        );
        set_new_script_template(&store, "").unwrap();
        assert_eq!(new_script_template(&store).unwrap(), "");
        let _ = std::fs::remove_dir_all(dir);
    }
}
