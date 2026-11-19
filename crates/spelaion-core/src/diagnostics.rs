//! Diagnostics for bug reports (SDB-398): the text "Copy diagnostic info" and a failed screen's
//! "Copy report" put on the clipboard. The app never sends it anywhere: the user copies it.
//!
//! Everything in it that does not come from the app itself (a plugin's state, a screen's error,
//! the app log's last lines) goes through [`Scrubber`]: the connection test's [`Redactor`]
//! (passwords, URL credentials, `Authorization` headers, `password=` values), the audit's
//! literal redaction (what the panic log uses), every text of every connection and network
//! profile file (names, hosts, users, databases, files), the computer's user name, the home
//! folder and any other absolute path, IP addresses and host names. Too much hidden is a report
//! that asks one more question; too little is a leak.

use std::path::{Path, PathBuf};

use crate::AppLog;
use crate::conn_test::{REDACTED, Redactor};

/// How many of the app log's last lines a report carries.
pub const LOG_LINES: usize = 40;

/// What stands for a host name or an IP address.
const HOST: &str = "<host>";
/// What stands for a path outside the app's own folders.
const PATH: &str = "<path>";
/// What stands for the personal folder (the app's own: its log, store and plugins).
const PERSONAL: &str = "<personal folder>";

/// Endings that make a dotted word a file name, not a host: source places of a panic
/// (`src/session.rs:12:5`) and the app's own files stay readable.
const FILE_ENDINGS: &[&str] = &[
    "rs", "ts", "tsx", "js", "mjs", "json", "toml", "log", "db", "sql", "md", "txt", "csv", "tsv",
    "parquet", "duckdb", "sqlite", "exe", "dll", "so", "dylib", "png", "svg", "html", "css", "yml",
    "yaml", "lock", "gz", "zip",
];

/// One plugin, as the Plugins page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLine {
    pub name: String,
    /// The build its engines start, if any.
    pub version: Option<String>,
    /// Where that build comes from, or why none starts.
    pub state: String,
}

/// A screen that failed (the page's error boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenFailure {
    /// Which screen: `explorer`, `tab`, `settings`…
    pub screen: String,
    /// The error's name and message.
    pub error: String,
    /// Where it happened: the components' stack.
    pub stack: String,
}

/// What a report is made of, besides the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsInput {
    pub app_version: String,
    pub plugins: Vec<PluginLine>,
    /// Engines with sessions open now, and how many: engine names only, never a connection's.
    pub engines_in_use: Vec<(String, u32)>,
    pub failure: Option<ScreenFailure>,
}

/// Takes out of a text what could tell who the user is or what they connect to.
pub struct Scrubber {
    redactor: Redactor,
    /// Folders replaced by a placeholder, longest first: `(folder, placeholder)`.
    folders: Vec<(String, &'static str)>,
}

impl Scrubber {
    /// A scrubber for this configuration: every text of its connection and network profile
    /// files is hidden, as are `user_dir` (shown as the personal folder), `home` (`~`) and the
    /// computer's `user_name`.
    pub fn new(
        config: &spelaion_config::Config,
        home: Option<&Path>,
        user_name: Option<&str>,
    ) -> Self {
        Self::from_parts(
            &config.connections,
            &config.tunnels,
            config.workspace_root.as_deref(),
            &config.user_dir,
            home,
            user_name,
        )
    }

    /// [`Scrubber::new`] from the configuration's parts.
    pub fn from_parts(
        connections: &[spelaion_config::ConnectionEntry],
        tunnels: &[spelaion_config::TunnelEntry],
        workspace_root: Option<&Path>,
        user_dir: &Path,
        home: Option<&Path>,
        user_name: Option<&str>,
    ) -> Self {
        let redactor = Redactor::new();
        let engines: Vec<&str> = spelaion_config::Engine::ALL
            .iter()
            .map(|engine| engine.as_str())
            .collect();
        let mut add = |value: &str| {
            let value = value.trim();
            // An engine's name stays (the report lists engines); a single character would only
            // blur the text.
            // The app's own name tells nothing about the user (the demo's bastions log in as
            // `spelaion`), and hiding it would hide the plugins' names and the log's file.
            let ours = ["spelaion", "spelaiondb"]
                .iter()
                .any(|name| value.eq_ignore_ascii_case(name));
            if value.chars().count() >= 2 && !engines.contains(&value) && !ours {
                redactor.add(value);
            }
        };
        for connection in connections {
            add(&connection.id.to_string());
            add(&connection.name());
            strings_of(&connection.file, &mut add);
            add(&connection.file_path.display().to_string());
        }
        for tunnel in tunnels {
            add(&tunnel.name);
            strings_of(&tunnel.file, &mut add);
            add(&tunnel.file_path.display().to_string());
        }
        if let Some(root) = workspace_root {
            add(&root.display().to_string());
        }
        if let Some(name) = user_name {
            add(name);
        }
        let mut folders = Vec::new();
        for (folder, placeholder) in [(Some(user_dir), PERSONAL), (home, "~")] {
            if let Some(folder) = folder {
                for spelling in spellings(folder) {
                    folders.push((spelling, placeholder));
                }
            }
        }
        folders.sort_by_key(|(folder, _)| std::cmp::Reverse(folder.len()));
        Self { redactor, folders }
    }

    /// `text` with nothing personal left: see the module's description.
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        // Folders first: the user name inside a home path becomes `~`, not `C:\Users\***`.
        for (folder, placeholder) in &self.folders {
            out = replace_ignoring_case(&out, folder, placeholder);
        }
        let out = self.redactor.redact(&out);
        let out = crate::audit::redact_secrets(&out);
        let out = scrub_absolute_paths(&out);
        scrub_hosts(&out)
    }
}

/// Every text value inside `value` once serialised (a connection or network profile file).
fn strings_of(value: &impl serde::Serialize, add: &mut impl FnMut(&str)) {
    fn walk(value: &serde_json::Value, add: &mut impl FnMut(&str)) {
        match value {
            serde_json::Value::String(text) => add(text),
            serde_json::Value::Array(items) => items.iter().for_each(|item| walk(item, add)),
            serde_json::Value::Object(fields) => fields.values().for_each(|field| walk(field, add)),
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
    if let Ok(value) = serde_json::to_value(value) {
        walk(&value, add);
    }
}

/// A folder as it may be written: as given, with the other slash, and with a slash at the end
/// removed.
fn spellings(folder: &Path) -> Vec<String> {
    let text = folder.display().to_string();
    let text = text.trim_end_matches(['/', '\\']).to_string();
    if text.chars().count() < 3 {
        return Vec::new();
    }
    let mut out = vec![
        text.clone(),
        text.replace('\\', "/"),
        text.replace('/', "\\"),
    ];
    out.sort();
    out.dedup();
    out
}

/// Every occurrence of `needle` in `text`, compared without case (Windows paths), replaced.
fn replace_ignoring_case(text: &str, needle: &str, replacement: &str) -> String {
    let lower_text = text.to_lowercase();
    let lower_needle = needle.to_lowercase();
    // Lower-casing changed a length: compare as written.
    if lower_text.len() != text.len() || lower_needle.len() != needle.len() {
        return text.replace(needle, replacement);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = lower_text[at..].find(&lower_needle) {
        out.push_str(&text[at..at + found]);
        out.push_str(replacement);
        at += found + needle.len();
    }
    out.push_str(&text[at..]);
    out
}

/// Characters that end a path or a host name inside a sentence.
fn ends_word(c: char) -> bool {
    c.is_whitespace() || "\"'`<>()[]{},;|".contains(c)
}

/// Absolute paths that are left (`C:\…`, `\\server\…`, `/home/…`) become [`PATH`]. A relative
/// path (a panic's `crates/…/src/x.rs:1:2`) stays.
fn scrub_absolute_paths(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        // Not right after a placeholder (`<personal folder>/logs`): that path is the app's.
        let starts_word =
            i == 0 || (ends_word(chars[i - 1]) && chars[i - 1] != '>') || chars[i - 1] == '=';
        let drive = starts_word
            && chars[i].is_ascii_alphabetic()
            && chars.get(i + 1) == Some(&':')
            && matches!(chars.get(i + 2), Some('\\' | '/'));
        let unc = starts_word && chars[i] == '\\' && chars.get(i + 1) == Some(&'\\');
        // A path inside the home folder, once that is `~`: its folders are the user's too.
        let home = starts_word && chars[i] == '~' && matches!(chars.get(i + 1), Some('\\' | '/'));
        // `/x/…`: a slash, a letter, and another slash later in the same word. Not `//`
        // (a URL's), which the host scrubbing handles.
        let unix = starts_word
            && chars[i] == '/'
            && chars
                .get(i + 1)
                .is_some_and(|c| c.is_alphanumeric() || *c == '.')
            && chars[i + 1..]
                .iter()
                .take_while(|c| !ends_word(**c))
                .any(|c| *c == '/');
        if drive || unc || unix || home {
            let mut end = i;
            while end < chars.len() && !ends_word(chars[end]) {
                end += 1;
            }
            // A place in a file (`…\x.rs:12:5`) keeps its line and column.
            let word: String = chars[i..end].iter().collect();
            out.push_str(PATH);
            out.push_str(place_of(&word));
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// The place after a path: `path:12:5` → `:12:5`; a path without one → ``.
fn place_of(word: &str) -> &str {
    let mut cut = word.len();
    let mut rest = word;
    for _ in 0..2 {
        match rest.rfind(':') {
            Some(at)
                if at + 1 < rest.len() && rest[at + 1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                cut = at;
                rest = &rest[..at];
            }
            _ => break,
        }
    }
    &word[cut..]
}

/// IP addresses and host names become [`HOST`]: a dotted word whose last part is letters
/// (`db.example.com`, `localhost.localdomain`) and that is not a file name, or four numbers
/// (`10.0.0.12`), or a bracketed IPv6 address. Versions (`0.10.0`) stay.
fn scrub_hosts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            out.push_str(&host_word(word));
            word.clear();
        }
    };
    for c in text.chars() {
        if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']' | ':' | '%') {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// One word, with any host or address in it hidden. A word may carry a port or a place after
/// a colon (`db.example.com:5432`, `src/x.rs:1:2`), kept as they are.
fn host_word(word: &str) -> String {
    if let Some(inside) = word.strip_prefix('[')
        && let Some(close) = inside.find(']')
        && inside[..close].contains(':')
    {
        return format!("{HOST}{}", &inside[close + 1..]);
    }
    let (name, rest) = match word.find(':') {
        // `scheme:` or `C:`-like words are not hosts.
        Some(at) if at > 0 => word.split_at(at),
        Some(_) | None => (word, ""),
    };
    let trimmed = name.trim_end_matches('.');
    if trimmed.eq_ignore_ascii_case("localhost") {
        return format!("{HOST}{}{rest}", &name[trimmed.len()..]);
    }
    let parts: Vec<&str> = trimmed.split('.').collect();
    if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
        return word.to_string();
    }
    let numbers = parts
        .iter()
        .all(|part| part.chars().all(|c| c.is_ascii_digit()));
    if numbers {
        // Four numbers is an IPv4 address; fewer is a version.
        return if parts.len() == 4 {
            format!("{HOST}{}{rest}", &name[trimmed.len()..])
        } else {
            word.to_string()
        };
    }
    let last = parts[parts.len() - 1];
    let letters = last.len() >= 2 && last.chars().all(|c| c.is_ascii_alphabetic());
    if !letters || FILE_ENDINGS.contains(&last.to_ascii_lowercase().as_str()) {
        return word.to_string();
    }
    format!("{HOST}{}{rest}", &name[trimmed.len()..])
}

/// The app log's last `lines` lines (both files when the log has just started over), oldest
/// first. A log that cannot be read gives none.
pub fn log_tail(log: &AppLog, lines: usize) -> Vec<String> {
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    let mut previous = log.path().as_os_str().to_owned();
    previous.push(".1");
    let text = format!("{}{}", read(&PathBuf::from(previous)), read(log.path()));
    let all: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|line| (*line).to_string())
        .collect()
}

/// The report: the app's version and system, its plugins, the engines in use, the failed
/// screen if any and the log's last lines, every outside text scrubbed.
pub fn report(input: &DiagnosticsInput, log: &[String], scrubber: &Scrubber) -> String {
    let mut out = String::new();
    out.push_str("SpelaionDB diagnostic info\n");
    out.push_str(&format!("Version: {}\n", input.app_version));
    out.push_str(&format!(
        "System: {} {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY
    ));
    if let Some(failure) = &input.failure {
        out.push_str("\nScreen that failed: ");
        out.push_str(&scrubber.scrub(one_line(&failure.screen)));
        out.push('\n');
        out.push_str(&format!(
            "Error: {}\n",
            scrubber.scrub(one_line(&failure.error))
        ));
        for line in failure
            .stack
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(30)
        {
            out.push_str(&format!("  {}\n", scrubber.scrub(line.trim())));
        }
    }
    out.push_str("\nPlugins:\n");
    if input.plugins.is_empty() {
        out.push_str("  none\n");
    }
    for plugin in &input.plugins {
        // A plugin's name and version are the pins' and the store's (checked), not the user's.
        let version = plugin.version.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "  {} {}: {}\n",
            one_line(&plugin.name),
            one_line(version),
            scrubber.scrub(one_line(&plugin.state))
        ));
    }
    out.push_str("\nEngines in use: ");
    if input.engines_in_use.is_empty() {
        out.push_str("none\n");
    } else {
        let engines: Vec<String> = input
            .engines_in_use
            .iter()
            .map(|(engine, sessions)| format!("{} ({sessions})", scrubber.scrub(engine)))
            .collect();
        out.push_str(&engines.join(", "));
        out.push('\n');
    }
    out.push_str(&format!("\nLast lines of the app log ({}):\n", log.len()));
    if log.is_empty() {
        out.push_str("  none\n");
    }
    for line in log {
        out.push_str(&format!("  {}\n", scrubber.scrub(line)));
    }
    out.push_str(&format!(
        "\nNames, hosts, users, file paths and secrets are replaced ({REDACTED}, {HOST}, {PATH}, ~).\n"
    ));
    out
}

/// The first line of `text`.
fn one_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_config::{ConnectionEntry, ConnectionFile, ConnectionId, Engine, Source};

    fn connection(id: &str, toml: &str) -> ConnectionEntry {
        let file: ConnectionFile = toml::from_str(toml).unwrap();
        ConnectionEntry {
            id: ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            },
            file_path: PathBuf::from(format!("C:\\Work\\acme\\.spelaion\\connections\\{id}.toml")),
            file,
        }
    }

    fn connections() -> Vec<ConnectionEntry> {
        vec![connection(
            "billing",
            r#"
                schema_version = 1
                name = "Billing replica"
                engine = "postgres"
                host = "pg-billing-7.internal.acme.example"
                database = "ledger_live"
                user = "svc_reports"
                tags = ["finance"]
            "#,
        )]
    }

    fn scrubber() -> Scrubber {
        Scrubber::from_parts(
            &connections(),
            &[],
            Some(Path::new("C:\\Work\\acme")),
            Path::new("C:\\Users\\ann\\.spelaion"),
            Some(Path::new("C:\\Users\\ann")),
            Some("ann"),
        )
    }

    fn input() -> DiagnosticsInput {
        DiagnosticsInput {
            app_version: "0.11.0".into(),
            plugins: vec![PluginLine {
                name: "spelaiondb-plugin-postgres".into(),
                version: Some("0.3.1".into()),
                state: "in use (pinned)".into(),
            }],
            engines_in_use: vec![("postgres".into(), 2), ("sqlite".into(), 1)],
            failure: None,
        }
    }

    /// Everything that must never reach a report.
    const LEAKS: &[&str] = &[
        "hunter2",
        "s3cr3t-T0ken",
        "pg-billing-7.internal.acme.example",
        "internal.acme.example",
        "10.20.30.40",
        "fe80::1",
        "Billing replica",
        "ledger_live",
        "svc_reports",
        "C:\\Users\\ann",
        "C:/Users/ann",
        "Users\\ann",
        "ann",
        "/home/bob",
        "Work\\acme",
        "acme",
        "finance",
        "db.partner.example.org",
        // Folders under the home folder are the user's too, and so is `localhost`'s use.
        "Documents",
        "AppData",
        "orders.sql",
        "localhost",
    ];

    fn assert_clean(report: &str) {
        let words: Vec<&str> = report.split(|c: char| !c.is_alphanumeric()).collect();
        for leak in LEAKS {
            // A short one as a word: "ann" is in "cannot".
            let leaked = if leak.chars().all(char::is_alphanumeric) && leak.len() < 5 {
                words.contains(leak)
            } else {
                report.contains(leak)
            };
            assert!(!leaked, "{leak:?} leaked into:\n{report}");
        }
    }

    #[test]
    fn a_report_keeps_the_version_the_plugins_and_the_engines() {
        let report = report(&input(), &[], &scrubber());
        assert!(report.contains("Version: 0.11.0"), "{report}");
        assert!(report.contains(std::env::consts::OS));
        assert!(
            report.contains("spelaiondb-plugin-postgres 0.3.1: in use (pinned)"),
            "{report}"
        );
        assert!(
            report.contains("Engines in use: postgres (2), sqlite (1)"),
            "{report}"
        );
        assert!(report.contains("Last lines of the app log (0):\n  none"));
    }

    #[test]
    fn passwords_hosts_and_home_folders_in_the_log_never_reach_the_report() {
        let log = vec![
            "1700000000000 could not connect to postgres://svc_reports:hunter2@pg-billing-7.internal.acme.example:5432/ledger_live".to_string(),
            "1700000000001 PGPASSWORD=hunter2 pg_dump --host 10.20.30.40".to_string(),
            "1700000000002 Authorization: Bearer s3cr3t-T0ken".to_string(),
            "1700000000003 password = 'hunter2' token=s3cr3t-T0ken".to_string(),
            "1700000000004 could not read C:\\Users\\ann\\Documents\\orders.sql: denied".to_string(),
            "1700000000005 could not read C:/Users/ann/Documents/orders.sql".to_string(),
            "1700000000006 could not open /home/bob/data/prod.duckdb".to_string(),
            "1700000000007 tunnel through [fe80::1]:22 and db.partner.example.org failed".to_string(),
            "1700000000008 connection Billing replica (billing) lost".to_string(),
            "1700000000009 opened C:\\Work\\acme\\.spelaion\\connections\\billing.toml for ann".to_string(),
            "1700000000010 tags: finance".to_string(),
            "1700000000013 kubectl port-forward to localhost:15432".to_string(),
            "1700000000011 internal error (panic) at crates/spelaion-core/src/session.rs:120:9, thread tokio-1: boom".to_string(),
            "1700000000012 wrote C:\\Users\\ann\\.spelaion\\logs\\spelaion.log".to_string(),
        ];
        let report = report(&input(), &log, &scrubber());
        assert_clean(&report);
        // What helps a report stays: the place of a panic, the app's own folder as a
        // placeholder, the version, ports.
        assert!(
            report.contains("crates/spelaion-core/src/session.rs:120:9"),
            "{report}"
        );
        assert!(
            report.contains("<personal folder>\\logs\\spelaion.log"),
            "{report}"
        );
        assert!(report.contains(":5432"), "{report}");
        assert!(report.contains("<host>"));
        assert!(report.contains("<path>"));
        assert!(report.contains("Version: 0.11.0"));
    }

    #[test]
    fn a_failed_screen_and_a_plugin_state_are_scrubbed_too() {
        let mut input = input();
        input.failure = Some(ScreenFailure {
            screen: "explorer".into(),
            error: "TypeError: cannot read 'host' of pg-billing-7.internal.acme.example (hunter2)\nsecond line".into(),
            stack: "at Row (C:\\Users\\ann\\AppData\\Local\\app.js:1:2)\nat Explorer (http://tauri.localhost/assets/index.js:3:4)".into(),
        });
        input.plugins.push(PluginLine {
            name: "spelaiondb-plugin-duckdb".into(),
            version: None,
            state:
                "missing: C:\\Users\\ann\\AppData\\Local\\SpelaionDB\\spelaiondb-plugin-duckdb.exe"
                    .into(),
        });
        let scrubber = scrubber();
        scrubber.redactor.add("hunter2");
        let report = report(&input, &[], &scrubber);
        assert_clean(&report);
        assert!(report.contains("Screen that failed: explorer"), "{report}");
        assert!(report.contains("TypeError: cannot read"), "{report}");
        assert!(!report.contains("second line"));
        assert!(
            report.contains("spelaiondb-plugin-duckdb -: missing: <path>"),
            "{report}"
        );
    }

    #[test]
    fn a_network_profile_s_bastions_and_users_are_hidden() {
        let file: spelaion_config::TunnelFile = toml::from_str(
            r#"
                schema_version = 1
                [[hops]]
                host = "jumpbox"
                user = "deploy_ops"
                auth = ["key"]
                identity_file = "keys/ops_ed25519"
            "#,
        )
        .unwrap();
        let tunnel = spelaion_config::TunnelEntry {
            source: Source::User,
            name: "prod-hops".into(),
            file_path: PathBuf::from("/home/bob/.spelaion/tunnels/prod-hops.toml"),
            file,
        };
        let scrubber = Scrubber::from_parts(
            &[],
            &[tunnel],
            None,
            Path::new("/home/bob/.spelaion"),
            Some(Path::new("/home/bob")),
            Some("bob"),
        );
        let line = "ssh to jumpbox as deploy_ops with keys/ops_ed25519 through prod-hops (bob)";
        let scrubbed = scrubber.scrub(line);
        for leak in ["jumpbox", "deploy_ops", "ops_ed25519", "prod-hops", "bob"] {
            assert!(!scrubbed.contains(leak), "{leak} in {scrubbed}");
        }
        assert_eq!(
            scrubber.scrub("/home/bob/.spelaion/logs/spelaion.log"),
            "<personal folder>/logs/spelaion.log"
        );
    }

    #[test]
    fn a_user_named_like_the_app_does_not_hide_the_app_s_own_words() {
        // The demo's bastions log in as `spelaion`.
        let file: spelaion_config::TunnelFile = toml::from_str(
            "schema_version = 1\n[[hops]]\nhost = \"bastion9\"\nuser = \"spelaion\"\nauth = [\"key\"]\n",
        )
        .unwrap();
        let tunnel = spelaion_config::TunnelEntry {
            source: Source::User,
            name: "demo-hops".into(),
            file_path: PathBuf::from("tunnels/demo-hops.toml"),
            file,
        };
        let scrubber = Scrubber::from_parts(&[], &[tunnel], None, Path::new("/tmp/u"), None, None);
        assert_eq!(
            scrubber.scrub("spelaiondb-plugin-duckdb wrote spelaion.log via bastion9"),
            "spelaiondb-plugin-duckdb wrote spelaion.log via ***"
        );
        let mut input = input();
        input.plugins[0].name = "spelaiondb-plugin-postgres".into();
        assert!(report(&input, &[], &scrubber).contains("  spelaiondb-plugin-postgres 0.3.1:"));
    }

    #[test]
    fn engine_names_and_versions_are_not_mistaken_for_secrets_or_hosts() {
        let scrubber = scrubber();
        for kept in [
            "postgres",
            "0.10.0",
            "v0.11.0",
            "spelaion.log",
            "src/x.rs:1:2",
            "Cargo.toml",
            "5432",
        ] {
            assert_eq!(scrubber.scrub(kept), kept);
        }
        assert_eq!(scrubber.scrub("db.example.com:5432"), "<host>:5432");
        assert_eq!(scrubber.scrub("192.168.1.10"), "<host>");
        assert_eq!(scrubber.scrub("at D:\\data\\x.duckdb:3:1"), "at <path>:3:1");
        assert!(
            Engine::ALL
                .iter()
                .all(|e| scrubber.scrub(e.as_str()) == e.as_str())
        );
    }

    #[test]
    fn the_log_tail_reads_the_last_lines_of_both_files() {
        let dir = std::env::temp_dir().join(format!("spelaion-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = AppLog::in_user_dir(&dir);
        log.append("first").unwrap();
        log.append("second").unwrap();
        log.append("third").unwrap();
        let tail = log_tail(&log, 2);
        assert_eq!(tail.len(), 2);
        assert!(
            tail[0].ends_with("second") && tail[1].ends_with("third"),
            "{tail:?}"
        );
        assert!(log_tail(&AppLog::in_user_dir(&dir.join("none")), 5).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
