//! Installing `spelaiondb mcp` in each AI client's file (ADR-033), on temporary directories:
//! a missing file, a file with other servers, our entry already there, and Codex's TOML. The
//! user's real files are never touched.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use spelaion_config::mcp_clients::{
    CliSource, ClientEnv, DiffLine, InstallState, McpClient, McpClientError, Os, ServerCommand,
    apply_install, apply_shown, claude_code_command, config_path, diff_text, find_cli,
    install_state, manual_config, plan_install,
};

fn temp(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "spelaion-clients-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn command() -> ServerCommand {
    ServerCommand::new(Path::new(r"C:\Program Files\SpelaionDB\spelaiondb.exe"))
}

const JSON_CLIENTS: [McpClient; 5] = [
    McpClient::ClaudeDesktop,
    McpClient::ClaudeCode,
    McpClient::Cursor,
    McpClient::Windsurf,
    McpClient::Antigravity,
];

/// Installs and returns the file's new text and the backup's.
fn install(client: McpClient, path: &Path) -> (String, Option<String>) {
    let plan = plan_install(client, path, &command()).unwrap();
    let backup = apply_install(&plan).unwrap();
    (
        std::fs::read_to_string(path).unwrap(),
        backup.map(|b| std::fs::read_to_string(b).unwrap()),
    )
}

fn entry(json: &serde_json::Value) -> &serde_json::Value {
    &json["mcpServers"]["spelaiondb"]
}

#[test]
fn paths_on_each_system() {
    let root = temp("paths");
    let windows = ClientEnv::under(&root, Os::Windows);
    let expect = |client, rel: &str| {
        assert_eq!(config_path(client, &windows), root.join(rel), "{client:?}");
    };
    expect(
        McpClient::ClaudeDesktop,
        "AppData/Roaming/Claude/claude_desktop_config.json",
    );
    expect(McpClient::ClaudeCode, ".claude.json");
    expect(McpClient::Codex, ".codex/config.toml");
    expect(McpClient::Cursor, ".cursor/mcp.json");
    expect(McpClient::Windsurf, ".codeium/windsurf/mcp_config.json");
    expect(
        McpClient::Antigravity,
        ".gemini/antigravity/mcp_config.json",
    );

    // The Microsoft Store's Claude Desktop reads its private copy first.
    let store = root.join("AppData/Local/Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude");
    std::fs::create_dir_all(&store).unwrap();
    assert_eq!(
        config_path(McpClient::ClaudeDesktop, &windows),
        store.join("claude_desktop_config.json")
    );

    let mac = ClientEnv::under(&root, Os::MacOs);
    assert_eq!(
        config_path(McpClient::ClaudeDesktop, &mac),
        root.join("Library/Application Support/Claude/claude_desktop_config.json")
    );
    let mut linux = ClientEnv::under(&root, Os::Linux);
    assert_eq!(
        config_path(McpClient::ClaudeDesktop, &linux),
        root.join(".config/Claude/claude_desktop_config.json")
    );
    linux.codex_home = Some(root.join("codex-home"));
    linux.claude_config_dir = Some(root.join("claude-dir"));
    assert_eq!(
        config_path(McpClient::Codex, &linux),
        root.join("codex-home/config.toml")
    );
    assert_eq!(
        config_path(McpClient::ClaudeCode, &linux),
        root.join("claude-dir/.claude.json")
    );
}

#[test]
fn a_missing_file_is_created_with_only_our_server() {
    for client in JSON_CLIENTS {
        let root = temp("missing");
        let path = config_path(client, &ClientEnv::under(&root, Os::Windows));
        assert_eq!(
            install_state(client, &path, &command()),
            InstallState::NoFile
        );
        let plan = plan_install(client, &path, &command()).unwrap();
        assert!(plan.before.is_none());
        assert!(
            plan.diff().iter().all(|l| matches!(l, DiffLine::Added(_))),
            "{}",
            diff_text(&plan.diff())
        );
        let (text, backup) = install(client, &path);
        assert!(backup.is_none(), "no file, no backup");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            entry(&json)["command"],
            r"C:\Program Files\SpelaionDB\spelaiondb.exe"
        );
        assert_eq!(entry(&json)["args"], serde_json::json!(["mcp"]));
        let servers = json["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        if client == McpClient::ClaudeCode {
            assert_eq!(entry(&json)["type"], "stdio");
        }
        assert_eq!(
            install_state(client, &path, &command()),
            InstallState::Installed
        );
        // Installing again changes nothing and leaves no backup.
        let again = plan_install(client, &path, &command()).unwrap();
        assert!(again.unchanged());
        assert_eq!(apply_install(&again).unwrap(), None);
    }

    let root = temp("missing-toml");
    let path = config_path(McpClient::Codex, &ClientEnv::under(&root, Os::Linux));
    let (text, _) = install(McpClient::Codex, &path);
    assert_eq!(
        text,
        "[mcp_servers.spelaiondb]\ncommand = 'C:\\Program Files\\SpelaionDB\\spelaiondb.exe'\nargs = [\"mcp\"]\n"
    );
}

#[test]
fn other_servers_and_keys_stay_byte_for_byte() {
    let original = "{\n    \"globalShortcut\": \"Ctrl+Space\",\n    \"mcpServers\": {\n        \"filesystem\": {\n            \"command\": \"npx\",\n            \"args\": [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"C:\\\\Users\\\\me\"],\n            \"env\": {\"TOKEN\": \"keep-me\"}\n        }\n    },\n    \"numbers\": [1.0, 2e3, -0]\n}\n";
    for client in JSON_CLIENTS {
        let root = temp("others");
        let path = root.join("config.json");
        std::fs::write(&path, original).unwrap();
        assert_eq!(
            install_state(client, &path, &command()),
            InstallState::NotInstalled
        );
        let plan = plan_install(client, &path, &command()).unwrap();
        // Only added lines, after the other server.
        let diff = plan.diff();
        assert!(
            !diff.iter().any(|l| matches!(l, DiffLine::Removed(_))),
            "{}",
            diff_text(&diff)
        );
        let (text, backup) = install(client, &path);
        assert_eq!(
            backup.as_deref(),
            Some(original),
            "the backup is the old file"
        );
        // The new text is the old one with our entry inserted, nothing else touched.
        let inserted_at = original.find("\n        }\n    },").unwrap() + "\n        }".len();
        assert_eq!(&text[..inserted_at], &original[..inserted_at], "{text}");
        let tail = &original[inserted_at..];
        assert!(text.ends_with(tail), "{text}");
        let added = &text[inserted_at..text.len() - tail.len()];
        assert!(
            added.starts_with(",\n        \"spelaiondb\": {\n            "),
            "{added:?}"
        );
        assert!(added.ends_with("\n        }"), "{added:?}");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["mcpServers"]["filesystem"]["env"]["TOKEN"], "keep-me");
        assert_eq!(json["globalShortcut"], "Ctrl+Space");
        assert_eq!(entry(&json)["args"], serde_json::json!(["mcp"]));
    }
}

#[test]
fn a_file_without_servers_gets_the_key_at_its_indentation() {
    let root = temp("no-servers");
    let path = root.join(".claude.json");
    // Claude Code's file: other keys, tabs, Windows line endings, a byte order mark.
    let original = "\u{feff}{\r\n\t\"numStartups\": 3,\r\n\t\"projects\": {\r\n\t\t\"C:/work\": {\"mcpServers\": {}}\r\n\t}\r\n}";
    std::fs::write(&path, original).unwrap();
    let (text, _) = install(McpClient::ClaudeCode, &path);
    assert!(
        text.starts_with("\u{feff}{\r\n\t\"numStartups\": 3,"),
        "{text:?}"
    );
    assert!(
        text.contains(
            "\t},\r\n\t\"mcpServers\": {\r\n\t\t\"spelaiondb\": {\r\n\t\t\t\"type\": \"stdio\",\r\n"
        ),
        "{text:?}"
    );
    assert!(
        !text.replace("\r\n", "").contains('\n'),
        "only CRLF: {text:?}"
    );
    let json: serde_json::Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap();
    // A project's own servers are not ours to change.
    assert_eq!(
        json["projects"]["C:/work"]["mcpServers"],
        serde_json::json!({})
    );
    assert_eq!(entry(&json)["type"], "stdio");

    // An empty object and a one-line file.
    for (original, expect_start) in [
        ("{}", "{\n  \"mcpServers\": {\n    \"spelaiondb\": {\n"),
        (
            "{\"a\":1}",
            "{\"a\":1, \"mcpServers\": {\n  \"spelaiondb\": {\n",
        ),
        (
            "{\"mcpServers\":{}}",
            "{\"mcpServers\":{\n  \"spelaiondb\": {\n",
        ),
        ("  \n", "{\n  \"mcpServers\""),
    ] {
        std::fs::write(&path, original).unwrap();
        let (text, _) = install(McpClient::Cursor, &path);
        assert!(text.starts_with(expect_start), "{original:?} → {text:?}");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(entry(&json)["args"], serde_json::json!(["mcp"]), "{text}");
    }
}

#[test]
fn an_existing_entry_is_updated_keeping_the_users_additions() {
    let root = temp("existing");
    let path = root.join("claude_desktop_config.json");
    let original = "{\n  \"mcpServers\": {\n    \"spelaiondb\": {\n      \"command\": \"C:\\\\old\\\\spelaiondb.exe\",\n      \"args\": [\"mcp\", \"--workspace\", \"C:\\\\repo\"],\n      \"env\": { \"SPELAION_USER_DIR\": \"D:\\\\me\" }\n    },\n    \"other\": { \"command\": \"x\" }\n  }\n}\n";
    std::fs::write(&path, original).unwrap();
    assert_eq!(
        install_state(McpClient::ClaudeDesktop, &path, &command()),
        InstallState::Different
    );
    let plan = plan_install(McpClient::ClaudeDesktop, &path, &command()).unwrap();
    let diff = diff_text(&plan.diff());
    assert!(
        diff.contains("-       \"command\": \"C:\\\\old\\\\spelaiondb.exe\","),
        "{diff}"
    );
    assert!(diff.contains("+       \"args\": [\"mcp\"],"), "{diff}");
    let (text, backup) = install(McpClient::ClaudeDesktop, &path);
    assert_eq!(backup.as_deref(), Some(original));
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(entry(&json)["env"]["SPELAION_USER_DIR"], "D:\\me");
    assert_eq!(entry(&json)["args"], serde_json::json!(["mcp"]));
    assert_eq!(json["mcpServers"]["other"]["command"], "x");
    assert_eq!(text.lines().count(), original.lines().count());

    // An entry that is not an object, and one missing fields (Claude Code's `type`).
    std::fs::write(&path, "{\"mcpServers\": {\"spelaiondb\": \"broken\"}}").unwrap();
    let (text, _) = install(McpClient::ClaudeCode, &path);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(entry(&json)["type"], "stdio");
    std::fs::write(
        &path,
        "{\n  \"mcpServers\": {\n    \"spelaiondb\": {\n      \"command\": \"old\"\n    }\n  }\n}\n",
    )
    .unwrap();
    let (text, _) = install(McpClient::ClaudeCode, &path);
    assert_eq!(
        text,
        "{\n  \"mcpServers\": {\n    \"spelaiondb\": {\n      \"command\": \"C:\\\\Program Files\\\\SpelaionDB\\\\spelaiondb.exe\",\n      \"type\": \"stdio\",\n      \"args\": [\"mcp\"]\n    }\n  }\n}\n"
    );
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(entry(&json)["type"], "stdio");
}

#[test]
fn codex_toml_keeps_comments_other_servers_and_extra_keys() {
    let root = temp("codex");
    let path = root.join("config.toml");
    let original = "# My Codex settings\nmodel = \"gpt-5\"  # the default\n\n[mcp_servers.github]\ncommand = \"npx\"\nargs = [\"-y\", \"github-mcp\"]\nenv = { GITHUB_TOKEN = \"keep-me\" }\n\n[profiles.fast]\nmodel = \"mini\"\n";
    std::fs::write(&path, original).unwrap();
    assert_eq!(
        install_state(McpClient::Codex, &path, &command()),
        InstallState::NotInstalled
    );
    let (text, backup) = install(McpClient::Codex, &path);
    assert_eq!(backup.as_deref(), Some(original));
    assert!(
        text.starts_with(
            "# My Codex settings\nmodel = \"gpt-5\"  # the default\n\n[mcp_servers.github]\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("env = { GITHUB_TOKEN = \"keep-me\" }"),
        "{text}"
    );
    assert!(
        text.contains("[profiles.fast]\nmodel = \"mini\"\n"),
        "{text}"
    );
    assert!(
        text.contains("[mcp_servers.spelaiondb]\ncommand = "),
        "{text}"
    );
    assert_eq!(
        install_state(McpClient::Codex, &path, &command()),
        InstallState::Installed
    );

    // An older entry: its command changes, the user's own keys stay.
    let older = "[mcp_servers.spelaiondb]\ncommand = \"/old/spelaiondb\"\nargs = [\"mcp\"]\nstartup_timeout_sec = 20 # slow disk\n";
    std::fs::write(&path, older).unwrap();
    assert_eq!(
        install_state(McpClient::Codex, &path, &command()),
        InstallState::Different
    );
    let (text, _) = install(McpClient::Codex, &path);
    assert!(
        text.contains("startup_timeout_sec = 20 # slow disk"),
        "{text}"
    );
    assert!(!text.contains("/old/spelaiondb"), "{text}");
    let parsed: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(
        parsed["mcp_servers"]["spelaiondb"]["command"].as_str(),
        Some(r"C:\Program Files\SpelaionDB\spelaiondb.exe")
    );
}

#[test]
fn files_that_do_not_parse_are_never_written() {
    let root = temp("broken");
    for (client, name, text) in [
        (McpClient::Cursor, "mcp.json", "{\"mcpServers\": {,}}"),
        // Comments are not JSON: the file is someone's hand-made one.
        (
            McpClient::Windsurf,
            "c.json",
            "{\n  // mine\n  \"mcpServers\": {}\n}",
        ),
        (McpClient::ClaudeDesktop, "d.json", "[1, 2]"),
        (McpClient::Antigravity, "e.json", "{\"mcpServers\": [1]}"),
        (McpClient::Codex, "config.toml", "[mcp_servers\ncommand = 1"),
        (McpClient::Codex, "other.toml", "mcp_servers = 3"),
    ] {
        let path = root.join(name);
        std::fs::write(&path, text).unwrap();
        let error = plan_install(client, &path, &command()).unwrap_err();
        assert!(
            matches!(
                error,
                McpClientError::Parse { .. } | McpClientError::Shape { .. }
            ),
            "{name}: {error:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!matches!(
            install_state(client, &path, &command()),
            InstallState::Installed
        ));
    }
}

#[test]
fn a_file_that_changed_after_the_diff_was_shown_is_not_written() {
    let root = temp("changed");
    let path = root.join("mcp.json");
    std::fs::write(&path, "{\"mcpServers\": {}}").unwrap();
    let plan = plan_install(McpClient::Cursor, &path, &command()).unwrap();
    let fingerprint = plan.fingerprint();
    // The client (or the user) saves it in the meantime.
    std::fs::write(&path, "{\"mcpServers\": {\"new\": {\"command\": \"y\"}}}").unwrap();
    assert!(matches!(
        apply_install(&plan),
        Err(McpClientError::Changed { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"mcpServers\": {\"new\": {\"command\": \"y\"}}}"
    );
    // A new plan has another fingerprint, so the change shown before cannot be confirmed.
    let replanned = plan_install(McpClient::Cursor, &path, &command()).unwrap();
    assert_ne!(replanned.fingerprint(), fingerprint);
    // And a file that appeared after a "create" plan is not overwritten.
    let fresh = root.join("fresh.json");
    let plan = plan_install(McpClient::Cursor, &fresh, &command()).unwrap();
    std::fs::write(&fresh, "{}").unwrap();
    assert!(matches!(
        apply_install(&plan),
        Err(McpClientError::Changed { .. })
    ));
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "{}");
    // No temporary file is left behind.
    assert!(std::fs::read_dir(&root).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".spelaion-tmp")
    }));
}

#[test]
fn backups_never_overwrite_each_other() {
    let root = temp("backups");
    let path = root.join("mcp.json");
    for n in 0..3 {
        std::fs::write(&path, format!("{{\"n\": {n}}}")).unwrap();
        install(McpClient::Cursor, &path);
    }
    let mut backups: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("mcp.json.spelaion-backup-"))
        .collect();
    backups.sort();
    assert_eq!(backups.len(), 3, "{backups:?}");
    let contents: Vec<String> = backups
        .iter()
        .map(|b| std::fs::read_to_string(root.join(b)).unwrap())
        .collect();
    for n in 0..3 {
        assert!(
            contents.contains(&format!("{{\"n\": {n}}}")),
            "{contents:?}"
        );
    }
}

#[test]
fn manual_configuration_and_the_claude_code_command() {
    let text = manual_config(McpClient::Cursor, &command());
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        entry(&json)["command"],
        r"C:\Program Files\SpelaionDB\spelaiondb.exe"
    );
    assert!(manual_config(McpClient::Codex, &command()).starts_with("[mcp_servers.spelaiondb]\n"));
    assert_eq!(
        claude_code_command(&command(), Os::Windows),
        r#"claude mcp add --scope user spelaiondb -- "C:\Program Files\SpelaionDB\spelaiondb.exe" mcp"#
    );
    assert_eq!(
        claude_code_command(
            &ServerCommand::new(Path::new("/Applications/Spelaion DB.app/x/spelaiondb")),
            Os::MacOs
        ),
        "claude mcp add --scope user spelaiondb -- '/Applications/Spelaion DB.app/x/spelaiondb' mcp"
    );
    assert_eq!(
        claude_code_command(
            &ServerCommand::new(Path::new("/usr/bin/spelaiondb")),
            Os::Linux
        ),
        "claude mcp add --scope user spelaiondb -- /usr/bin/spelaiondb mcp"
    );
}

#[test]
fn a_big_file_with_a_small_change_shows_a_short_diff() {
    let root = temp("big");
    let path = root.join(".claude.json");
    let mut original = String::from("{\n  \"projects\": {\n");
    for n in 0..20_000 {
        original.push_str(&format!(
            "    \"p{n}\": {{ \"history\": [\"select {n}\"] }},\n"
        ));
    }
    original.push_str("    \"last\": {}\n  }\n}\n");
    std::fs::write(&path, &original).unwrap();
    let plan = plan_install(McpClient::ClaudeCode, &path, &command()).unwrap();
    let diff = plan.diff();
    assert!(diff.len() < 20, "{}", diff_text(&diff));
    assert!(matches!(diff[0], DiffLine::Gap(n) if n > 19_000));
}

#[test]
fn only_the_change_that_was_shown_is_written() {
    let root = temp("shown");
    let path = root.join("mcp.json");
    std::fs::write(&path, "{\"mcpServers\": {}}").unwrap();
    let plan = plan_install(McpClient::Cursor, &path, &command()).unwrap();
    // Another command than the one shown (the app moved) is another change.
    let other = ServerCommand::new(Path::new("/elsewhere/spelaiondb"));
    assert!(matches!(
        apply_shown(McpClient::Cursor, &path, &other, &plan.fingerprint()),
        Err(McpClientError::Changed { .. })
    ));
    assert!(matches!(
        apply_shown(McpClient::Cursor, &path, &command(), "0000000000000000"),
        Err(McpClientError::Changed { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"mcpServers\": {}}"
    );
    let backup = apply_shown(McpClient::Cursor, &path, &command(), &plan.fingerprint()).unwrap();
    assert!(backup.is_some());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), plan.after);
}

#[test]
fn the_cli_is_found_next_to_the_app_then_on_the_path() {
    let root = temp("cli");
    let app_dir = root.join("app");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    let app = app_dir.join("spelaion-desktop.exe");
    let path = std::env::join_paths([bin_dir.clone()]).unwrap();
    // Nothing anywhere.
    assert_eq!(
        find_cli(Some(&app), None, Some(&path), false, Os::Windows),
        None
    );
    // On the PATH.
    std::fs::write(bin_dir.join("spelaiondb.exe"), b"").unwrap();
    assert_eq!(
        find_cli(Some(&app), None, Some(&path), false, Os::Windows),
        Some((bin_dir.join("spelaiondb.exe"), CliSource::Path))
    );
    // Next to the app wins (what the installers ship).
    std::fs::write(app_dir.join("spelaiondb.exe"), b"").unwrap();
    assert_eq!(
        find_cli(Some(&app), None, Some(&path), false, Os::Windows),
        Some((app_dir.join("spelaiondb.exe"), CliSource::Bundled))
    );
    // An AppImage's neighbour is in a mount that changes every run.
    assert_eq!(
        find_cli(Some(&app), None, Some(&path), true, Os::Windows),
        Some((bin_dir.join("spelaiondb.exe"), CliSource::Path))
    );
    // The override wins, and only if it exists.
    let chosen = root.join("chosen.exe");
    assert_eq!(
        find_cli(Some(&app), Some(&chosen), Some(&path), false, Os::Windows),
        None
    );
    std::fs::write(&chosen, b"").unwrap();
    assert_eq!(
        find_cli(Some(&app), Some(&chosen), Some(&path), false, Os::Windows),
        Some((chosen, CliSource::Override))
    );
    // A relative PATH entry is never trusted.
    let relative = std::env::join_paths([PathBuf::from("bin")]).unwrap();
    assert_eq!(
        find_cli(None, None, Some(&relative), false, Os::Windows),
        None
    );
}
