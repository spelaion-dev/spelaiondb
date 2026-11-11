//! Column masking rules (ADR-051): where they come from, how they combine, and every way a
//! file could try to unmask a column: a repository's `never`, a team type that drops its
//! `mask_columns`, a `user.toml` that does not load, pasted text and the connection form.
//! Temporary directories and a fake secret backend: nothing real is touched.

use std::path::PathBuf;

use spelaion_config::{
    ConnectionDraft, ConnectionId, MASK_EVERYTHING, MaskingConfig, ResolveError,
    ResolvedConnection, Resolver, SecretBackend, Source, TrustStore, load, paste_connection,
    update_connection,
};

struct Fixture {
    base: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-masking-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn repo(&self) -> PathBuf {
        self.base.join("repo")
    }

    fn user(&self) -> PathBuf {
        self.base.join("user")
    }

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.base.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn trusted(&self) -> TrustStore {
        let mut trust = TrustStore::load(&self.user()).unwrap();
        trust
            .trust(&load(Some(&self.repo()), &self.user()))
            .unwrap();
        trust
    }

    fn resolve(
        &self,
        trust: &TrustStore,
        id: &ConnectionId,
    ) -> Result<ResolvedConnection, ResolveError> {
        let config = load(Some(&self.repo()), &self.user());
        Resolver::new(&config, trust, &NoSecrets).resolve(id)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct NoSecrets;

impl SecretBackend for NoSecrets {
    fn keychain_get(&self, _: &str) -> Result<Option<String>, String> {
        panic!("no secret is read here")
    }
    fn keychain_set(&self, _: &str, _: &str) -> Result<(), String> {
        panic!("no secret is written here")
    }
    fn keychain_delete(&self, _: &str) -> Result<(), String> {
        panic!("no secret is deleted here")
    }
    fn env(&self, _: &str) -> Option<String> {
        None
    }
    fn run(&self, _: &str, _: &[&str]) -> Result<String, String> {
        panic!("nothing is run here")
    }
    fn run_shell(&self, _: &str) -> Result<String, String> {
        panic!("nothing is run here")
    }
}

fn ws(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::Workspace,
        path: path.into(),
    }
}

fn personal(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::User,
        path: path.into(),
    }
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

const CONNECTION: &str = "schema_version = 1\nengine = \"sqlite\"\nenv = \"pii\"\ndatabase = \"app.db\"\n\n[masking]\nalways = [\"*email*\"]\nnever = [\"email_verified\"]\n";

const TEAM_TYPES: &str =
    "schema_version = 1\n\n[types.pii]\nname = \"PII\"\nmask_columns = [\"ssn\", \"card_*\"]\n";

#[test]
fn rules_combine_from_the_connection_the_user_and_the_type() {
    let fx = Fixture::new("combine");
    fx.write(
        "user/user.toml",
        "schema_version = 1\n[masking]\nalways = [\"*password*\"]\n",
    );
    fx.write(
        "user/connection-types.toml",
        "schema_version = 1\n[types.pii]\nmask_columns = [\"phone\"]\n",
    );
    fx.write("repo/.spelaion/connection-types.toml", TEAM_TYPES);
    fx.write("user/connections/mine.toml", CONNECTION);
    fx.write("repo/.spelaion/connections/team.toml", CONNECTION);
    let trust = fx.trusted();

    // Personal: the user's patterns can be exempted by the connection's `never`.
    let mine = fx.resolve(&trust, &personal("mine")).unwrap().masking;
    assert_eq!(mine.required, v(&["ssn", "card_*", "phone"]));
    assert_eq!(mine.exemptible, v(&["*email*", "*password*"]));
    assert_eq!(mine.never, v(&["email_verified"]));

    // From the repository: the user's patterns are required there.
    let team = fx.resolve(&trust, &ws("team")).unwrap().masking;
    assert_eq!(team.required, v(&["ssn", "card_*", "phone", "*password*"]));
    assert_eq!(team.exemptible, v(&["*email*"]));
}

#[test]
fn every_way_a_repository_could_unmask_a_column_is_reviewed() {
    let fx = Fixture::new("review");
    let types = fx.write("repo/.spelaion/connection-types.toml", TEAM_TYPES);
    let connection = fx.write("repo/.spelaion/connections/team.toml", CONNECTION);
    let trust = fx.trusted();
    fx.resolve(&trust, &ws("team")).unwrap();
    for (what, file, edited) in [
        (
            "a never added",
            &connection,
            CONNECTION.replace("never = [\"email_verified\"]", "never = [\"*\"]"),
        ),
        (
            "an always removed",
            &connection,
            CONNECTION.replace("always = [\"*email*\"]\n", ""),
        ),
        (
            "the whole table removed",
            &connection,
            CONNECTION.split("\n[masking]").next().unwrap().to_string(),
        ),
        (
            "the type's patterns narrowed",
            &types,
            TEAM_TYPES.replace("[\"ssn\", \"card_*\"]", "[\"ssn\"]"),
        ),
        (
            "the type's patterns removed",
            &types,
            TEAM_TYPES.replace("mask_columns = [\"ssn\", \"card_*\"]\n", ""),
        ),
    ] {
        let before = std::fs::read_to_string(file).unwrap();
        std::fs::write(file, &edited).unwrap();
        match fx.resolve(&trust, &ws("team")) {
            Err(ResolveError::Changed { .. }) => {}
            other => panic!("{what}: {:?}", other.map(|c| c.masking)),
        }
        std::fs::write(file, before).unwrap();
        fx.resolve(&trust, &ws("team")).unwrap();
    }
}

#[test]
fn a_user_file_that_does_not_load_masks_everything() {
    let fx = Fixture::new("broken-user");
    fx.write(
        "user/connections/mine.toml",
        CONNECTION.replace("env = \"pii\"\n", "").as_str(),
    );
    let trust = TrustStore::load(&fx.user()).unwrap();
    for broken in [
        "schema_version = 1\n[masking\n",
        "schema_version = 1\n[masking]\nalways = [\"\"]\n",
        "schema_version = 1\n[masking]\nalways = [\"e\u{202E}mail\"]\n",
        "schema_version = 1\n[masking]\nnever = [\"x\"]\n",
    ] {
        fx.write("user/user.toml", broken);
        let rules = fx.resolve(&trust, &personal("mine")).unwrap().masking;
        assert!(
            rules.required.iter().any(|p| p == MASK_EVERYTHING),
            "{broken:?}: {rules:?}"
        );
    }
    fx.write("user/user.toml", "schema_version = 1\n");
    let rules = fx.resolve(&trust, &personal("mine")).unwrap().masking;
    assert!(rules.required.is_empty());
}

#[test]
fn masking_patterns_are_validated_where_they_load() {
    let fx = Fixture::new("validate");
    let long = "x".repeat(129);
    let many = (0..101)
        .map(|i| format!("\"c{i}\""))
        .collect::<Vec<_>>()
        .join(", ");
    for bad in [
        "always = [\"\"]".to_string(),
        "never = [\"  \"]".to_string(),
        format!("always = [\"{long}\"]"),
        format!("always = [{many}]"),
        "always = [\"a\\u0000b\"]".to_string(),
        "mask = [\"email\"]".to_string(),
    ] {
        fx.write(
            "user/connections/bad.toml",
            &format!(
                "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"x.db\"\n[masking]\n{bad}\n"
            ),
        );
        let config = load(None, &fx.user());
        assert!(
            config.connection(&personal("bad")).is_none(),
            "{bad}: loaded anyway"
        );
    }
    fx.write(
        "user/connection-types.toml",
        "schema_version = 1\n[types.pii]\nmask_columns = [\"\"]\n",
    );
    let config = load(None, &fx.user());
    assert!(config.user_types.file().is_none());
}

#[test]
fn a_pasted_connection_can_add_masking_but_never_take_it_away() {
    let fx = Fixture::new("paste");
    fx.write(
        "user/user.toml",
        "schema_version = 1\n[masking]\nalways = [\"*email*\"]\n",
    );
    let config = load(None, &fx.user());
    let pasted = paste_connection(
        &config,
        "schema_version = 1\nname = \"From chat\"\nengine = \"sqlite\"\ndatabase = \"x.db\"\n[masking]\nalways = [\"ssn\"]\nnever = [\"*email*\", \"*\"]\n",
        &NoSecrets,
    )
    .unwrap();
    assert!(pasted.left_out.iter().any(|f| f == "masking.never"));
    let trust = TrustStore::load(&fx.user()).unwrap();
    let rules = fx.resolve(&trust, &pasted.id).unwrap().masking;
    assert!(rules.never.is_empty(), "{rules:?}");
    assert!(rules.exemptible.contains(&"ssn".to_string()));
    assert!(rules.exemptible.contains(&"*email*".to_string()));
}

#[test]
fn the_form_keeps_the_rules_when_a_connection_is_edited() {
    let fx = Fixture::new("form");
    fx.write("user/connections/mine.toml", CONNECTION);
    let config = load(None, &fx.user());
    let entry = config.connection(&personal("mine")).unwrap();
    let mut draft = ConnectionDraft::from_entry(entry);
    assert_eq!(
        draft.masking,
        MaskingConfig {
            always: v(&["*email*"]),
            never: v(&["email_verified"]),
        }
    );
    // Something else changes: the rules stay as they were.
    draft.description = "Customers".into();
    update_connection(&config, &personal("mine"), &draft, None, &NoSecrets).unwrap();
    let text = std::fs::read_to_string(fx.user().join("connections/mine.toml")).unwrap();
    assert!(text.contains("always = [\"*email*\"]"), "{text}");
    assert!(text.contains("never = [\"email_verified\"]"), "{text}");
    // Patterns typed in the form are trimmed, blank ones left out, each kept once.
    draft.masking.always = v(&["  card_* ", "", "card_*", "*email*"]);
    let config = load(None, &fx.user());
    update_connection(&config, &personal("mine"), &draft, None, &NoSecrets).unwrap();
    let config = load(None, &fx.user());
    assert_eq!(
        config
            .connection(&personal("mine"))
            .unwrap()
            .file
            .masking
            .always,
        v(&["card_*", "*email*"])
    );
}
