//! The settings file (SDB-31): exported on one computer and imported on another, every portable
//! setting arrives, the personal files only add, nothing of the first computer's folders moves,
//! and a file that would be refused from Settings changes nothing.

use std::path::{Path, PathBuf};

use spelaion_core::{
    QueryHistory, SettingsTransferError, export_settings, import_settings, preview_settings,
};

struct Machine {
    dir: PathBuf,
    history: QueryHistory,
}

impl Machine {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-settings-transfer-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let history = QueryHistory::open(dir.join("state.db")).unwrap();
        Self { dir, history }
    }

    fn user_dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A folder of the old computer, absolute on every system (a recent folder must be).
fn old_folder() -> String {
    std::env::temp_dir()
        .join("spelaion-old-work")
        .display()
        .to_string()
}

async fn configure(old: &Machine) {
    let history = &old.history;
    history.set_theme("dark".into()).await.unwrap();
    history.set_language("es".into()).await.unwrap();
    history.set_interface_size(110).await.unwrap();
    history.add_recent_folder(old_folder()).await.unwrap();
    history
        .set_connection_name_pattern(Some("{engine} {host}".into()))
        .await
        .unwrap();
    history
        .set_shortcut_overrides(vec![
            spelaion_core::ShortcutOverride {
                command: "editor.run".into(),
                keys: Some("Ctrl+Alt+F9".into()),
            },
            spelaion_core::ShortcutOverride {
                command: "tab.reopen".into(),
                keys: None,
            },
        ])
        .await
        .unwrap();
    history
        .save_editor_snippet(spelaion_core::EditorSnippet {
            name: "cnt".into(),
            description: "Count rows".into(),
            body: "select count(*) from ${1:table}".into(),
        })
        .await
        .unwrap();
    history
        .set_new_script_template("-- new script\n".into())
        .await
        .unwrap();
    history.set_mcp_read_only(true).await.unwrap();
    std::fs::write(
        old.user_dir().join("user.toml"),
        "schema_version = 1\n\n[vars]\ndb_user = \"pablo\"\n\n[masking]\nalways = [\"*.email\"]\n",
    )
    .unwrap();
    std::fs::write(
        old.user_dir().join("connection-types.toml"),
        "schema_version = 1\n\n[types.staging]\nname = \"Staging\"\nconfirm_data_changes = true\n",
    )
    .unwrap();
}

#[tokio::test]
async fn settings_exported_on_one_computer_arrive_whole_on_another() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = Machine::new("round-trip-old");
    configure(&old).await;
    let file = old.dir.join("settings.spelaion-settings");
    let exported = export_settings(&old.history, old.user_dir(), &file, "0.18.0")
        .await
        .unwrap();
    assert_eq!(exported.shortcuts, 2);
    assert_eq!(exported.snippets, 1);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        !text.contains(&old_folder().replace(std::path::MAIN_SEPARATOR, "/"))
            && !text.contains(&old_folder()),
        "a folder of this computer moved"
    );

    let new = Machine::new("round-trip-new");
    let preview = preview_settings(&new.history, new.user_dir(), &file)
        .await
        .unwrap();
    assert_eq!(preview.variables, vec!["db_user"]);
    assert_eq!(preview.masking_added, vec!["*.email"]);
    assert_eq!(preview.types_added, vec!["staging"]);
    assert!(!preview.mcp_read_only_kept);
    assert!(preview.skipped.is_empty());
    // A preview writes nothing.
    assert_eq!(new.history.theme().await.unwrap(), "system");
    assert!(!new.user_dir().join("user.toml").exists());

    import_settings(&new.history, new.user_dir(), &file)
        .await
        .unwrap();
    assert_eq!(
        new.history.portable_settings().await.unwrap(),
        old.history.portable_settings().await.unwrap()
    );
    assert_eq!(new.history.theme().await.unwrap(), "dark");
    assert_eq!(new.history.interface_size().await.unwrap(), 110);
    assert_eq!(new.history.shortcut_overrides().await.unwrap().len(), 2);
    assert_eq!(new.history.editor_snippets().await.unwrap().len(), 1);
    assert!(new.history.mcp_settings().await.unwrap().0);
    assert!(new.history.recent_folders().await.unwrap().is_empty());
    let config = spelaion_config::load(None, new.user_dir());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    assert_eq!(config.user.vars.get("db_user").unwrap(), "pablo");
    assert_eq!(config.user.masking.always, vec!["*.email"]);
    assert!(config.user_types.file().unwrap().types["staging"].confirm_data_changes);
}

#[tokio::test]
async fn an_import_keeps_read_only_agents_and_a_refused_file_changes_nothing() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = Machine::new("refused-old");
    old.history.set_theme("light".into()).await.unwrap();
    let file = old.dir.join("settings.spelaion-settings");
    export_settings(&old.history, old.user_dir(), &file, "0.18.0")
        .await
        .unwrap();

    let new = Machine::new("refused-new");
    new.history.set_mcp_read_only(true).await.unwrap();
    new.history.set_theme("dark".into()).await.unwrap();
    let done = import_settings(&new.history, new.user_dir(), &file)
        .await
        .unwrap();
    assert!(done.mcp_read_only_kept);
    assert!(new.history.mcp_settings().await.unwrap().0);
    assert_eq!(new.history.theme().await.unwrap(), "light");

    // Edited by hand: a key the editor keeps for itself. Nothing changes, the files included.
    let text = std::fs::read_to_string(&file).unwrap();
    let edited = text.replace(
        "\"settings\": {",
        "\"settings\": {\n    \"shortcut:editor.run\": \"Alt+F4\",",
    );
    assert_ne!(edited, text);
    let bad = new.dir.join("edited.spelaion-settings");
    std::fs::write(&bad, edited).unwrap();
    new.history.set_theme("dark".into()).await.unwrap();
    let error = import_settings(&new.history, new.user_dir(), &bad)
        .await
        .unwrap_err();
    assert!(
        matches!(error, SettingsTransferError::Refused { .. }),
        "{error}"
    );
    assert_eq!(new.history.theme().await.unwrap(), "dark");

    std::fs::write(&bad, b"{\"format\": \"something else\"}").unwrap();
    assert!(matches!(
        preview_settings(&new.history, new.user_dir(), &bad).await,
        Err(SettingsTransferError::NotASettingsFile)
    ));
}
