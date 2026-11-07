//! Connection types (ADR-029): how the layers combine, how the team's file follows repository
//! trust, and every way a file could try to loosen a safeguard. Temporary directories and a
//! fake secret backend: the real keychain and environment are never touched.

use std::path::PathBuf;

use spelaion_config::{
    ConnectionDraft, ConnectionId, ConnectionTypeConfig, Engine, NamePattern, NameValues,
    ResolveError, ResolvedConnection, Resolver, SecretBackend, Severity, Source, Tone, TrustStore,
    check_workspace, connection_clipboard_text, duplicate_connection, load, paste_connection,
    remove_personal_type, set_personal_type,
};

struct Fixture {
    base: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-types-{name}-{}", std::process::id()));
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

/// These connections have no secrets: nothing may be read.
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

fn sqlite(env: &str) -> String {
    format!("schema_version = 1\nengine = \"sqlite\"\nenv = \"{env}\"\ndatabase = \"app.db\"\n")
}

const TEAM: &str = "schema_version = 1

[types.critical]
name = \"Critical\"
tone = \"red\"
description = \"Money moves here\"
confirm_execution = true

[types.staging]
name = \"Staging\"
tone = \"green\"
";

fn changed(result: Result<ResolvedConnection, ResolveError>) -> spelaion_config::ItemChange {
    match result {
        Err(ResolveError::Changed { change, .. }) => change,
        other => panic!("expected a change to review: {:?}", other.map(|c| c.kind)),
    }
}

#[test]
fn team_types_apply_only_from_a_trusted_repository_and_as_reviewed() {
    let fx = Fixture::new("team");
    let team = fx.write("repo/.spelaion/connection-types.toml", TEAM);
    fx.write(
        "repo/.spelaion/connections/ledger.toml",
        &sqlite("critical"),
    );
    fx.write("repo/.spelaion/connections/qa.toml", &sqlite("staging"));
    fx.write("user/connections/mine.toml", &sqlite("Critical"));

    // Not trusted: the team's types do not exist for the user's own connections either.
    let untrusted = TrustStore::load(&fx.user()).unwrap();
    assert!(matches!(
        fx.resolve(&untrusted, &ws("ledger")),
        Err(ResolveError::Untrusted { .. })
    ));
    assert_eq!(
        fx.resolve(&untrusted, &personal("mine")).unwrap().kind,
        None
    );

    let mut trust = fx.trusted();
    let ledger = fx.resolve(&trust, &ws("ledger")).unwrap();
    let kind = ledger.kind.clone().unwrap();
    assert_eq!(kind.name, "Critical");
    assert!(kind.team && !kind.built_in);
    assert!(ledger.safeguards().confirm_execution);
    assert!(!ledger.is_production());
    // A personal connection uses it too, by any case.
    assert!(
        fx.resolve(&trust, &personal("mine"))
            .unwrap()
            .safeguards()
            .confirm_execution
    );

    // The badge's tone and the description need no review.
    std::fs::write(
        &team,
        TEAM.replace("tone = \"red\"", "tone = \"blue\"")
            .replace("Money moves here", "Payments"),
    )
    .unwrap();
    let ledger = fx.resolve(&trust, &ws("ledger")).unwrap();
    assert_eq!(ledger.kind.unwrap().tone, Tone::Blue);

    // Switching the safeguard off waits for the user's review; types it does not touch
    // keep working.
    std::fs::write(
        &team,
        TEAM.replace("confirm_execution = true", "confirm_execution = false"),
    )
    .unwrap();
    let change = changed(fx.resolve(&trust, &ws("ledger")));
    assert_eq!(change.item, "connection_types");
    assert_eq!(
        change.fields,
        [spelaion_config::FieldChange {
            field: "types.critical.confirm_execution".into(),
            before: Some("true".into()),
            after: Some("false".into()),
        }]
    );
    assert!(changed(fx.resolve(&trust, &personal("mine"))).item == "connection_types");
    assert_eq!(
        fx.resolve(&trust, &ws("qa")).unwrap().kind.unwrap().name,
        "Staging"
    );
    let message = ResolveError::Changed {
        root: fx.repo(),
        change: change.clone(),
    }
    .to_string();
    assert!(
        message.contains(".spelaion/connection-types.toml"),
        "{message}"
    );
    // The review lists it with the other changes of the repository.
    let config = load(Some(&fx.repo()), &fx.user());
    assert_eq!(trust.changes(&config), std::slice::from_ref(&change));
    // Accepting exactly what was shown applies it.
    trust.accept_reviewed(&config, &fx.repo(), &change).unwrap();
    assert!(
        !fx.resolve(&trust, &ws("ledger"))
            .unwrap()
            .safeguards()
            .confirm_execution
    );
}

#[test]
fn every_way_a_team_file_could_drop_a_safeguard_is_reviewed() {
    let fx = Fixture::new("team-ways");
    let team = fx.write("repo/.spelaion/connection-types.toml", TEAM);
    fx.write(
        "repo/.spelaion/connections/ledger.toml",
        &sqlite("critical"),
    );
    let trust = fx.trusted();
    fx.resolve(&trust, &ws("ledger")).unwrap();
    for (what, edited) in [
        (
            "renamed to look like another type",
            TEAM.replace("name = \"Critical\"", "name = \"Development\""),
        ),
        (
            "removed, so the label becomes a plain one",
            TEAM.replace("[types.critical]", "[types.other]"),
        ),
        (
            "an idle time added (a new setting is a change too)",
            TEAM.replace(
                "confirm_execution = true",
                "confirm_execution = true\nclose_idle_after = 60",
            ),
        ),
        (
            "the id changed case (ids are lowercase only)",
            TEAM.replace("[types.critical]", "[types.Critical]"),
        ),
    ] {
        std::fs::write(&team, &edited).unwrap();
        let result = fx.resolve(&trust, &ws("ledger"));
        assert!(result.is_err(), "{what}: {:?}", result.map(|c| c.kind));
    }

    // The whole file deleted: its types no longer apply, so it is a change.
    std::fs::remove_file(&team).unwrap();
    let change = changed(fx.resolve(&trust, &ws("ledger")));
    assert!(
        change
            .fields
            .iter()
            .all(|f| f.after.is_none() && f.field.starts_with("types."))
    );

    // A file that does not load stops every typed connection until it is fixed.
    std::fs::write(&team, "schema_version = 1\n[types.critical\n").unwrap();
    let error = fx.resolve(&trust, &ws("ledger")).unwrap_err().to_string();
    assert!(
        error.contains("connection types could not be read"),
        "{error}"
    );
    fx.write(
        "repo/.spelaion/connections/plain.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"x.db\"\n",
    );
    // A repository change to review, but nothing about types: the user's untyped connection
    // still opens.
    fx.write(
        "user/connections/untyped.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"x.db\"\n",
    );
    fx.resolve(&trust, &personal("untyped")).unwrap();
}

#[test]
fn a_new_team_type_waits_for_review_only_where_it_is_used() {
    let fx = Fixture::new("team-new");
    fx.write(
        "repo/.spelaion/connections/ledger.toml",
        &sqlite("critical"),
    );
    fx.write("repo/.spelaion/connections/dev.toml", &sqlite("dev"));
    let mut trust = fx.trusted();
    // Before the team defines it, `critical` is a plain label.
    assert_eq!(fx.resolve(&trust, &ws("ledger")).unwrap().kind, None);
    fx.write("repo/.spelaion/connection-types.toml", TEAM);
    let change = changed(fx.resolve(&trust, &ws("ledger")));
    assert!(change.new);
    assert_eq!(
        fx.resolve(&trust, &ws("dev")).unwrap().kind.unwrap().name,
        "Development"
    );
    let config = load(Some(&fx.repo()), &fx.user());
    trust.accept(&config, &change.item).unwrap();
    assert!(
        fx.resolve(&trust, &ws("ledger"))
            .unwrap()
            .safeguards()
            .confirm_execution
    );
}

#[test]
fn production_can_be_tightened_but_never_loosened() {
    let fx = Fixture::new("production");
    // The team tries to turn Production's confirmation off and rename it; the user's own file
    // tries too. Writing `false` is the same as not writing it.
    fx.write(
        "repo/.spelaion/connection-types.toml",
        "schema_version = 1\n[types.prod]\nname = \"Prod EU\"\ntone = \"green\"\nconfirm_data_changes = false\n",
    );
    fx.write(
        "user/connection-types.toml",
        "schema_version = 1\n[types.prod]\nconfirm_data_changes = false\nconfirm_execution = true\n",
    );
    for (name, env) in [("a", "prod"), ("b", "PRODUCTION"), ("c", " Prod ")] {
        fx.write(
            &format!("repo/.spelaion/connections/{name}.toml"),
            &sqlite(env),
        );
    }
    fx.write("repo/.spelaion/connections/d.toml", &sqlite("prodlike"));
    let trust = fx.trusted();
    for name in ["a", "b", "c"] {
        let resolved = fx.resolve(&trust, &ws(name)).unwrap();
        assert!(resolved.is_production(), "{name}");
        let safeguards = resolved.safeguards();
        assert!(safeguards.confirm_data_changes, "{name}");
        assert!(
            safeguards.confirm_execution,
            "{name}: the user tightened it"
        );
        assert_eq!(resolved.kind.unwrap().name, "Prod EU");
    }
    // Only the core decides what is Production: `prodlike` is a plain label.
    let prodlike = fx.resolve(&trust, &ws("d")).unwrap();
    assert_eq!(prodlike.kind, None);
    assert!(!prodlike.safeguards().confirm_data_changes);

    // `production` cannot be defined as a type of its own beside Production.
    let path = fx.write(
        "user/connection-types.toml",
        "schema_version = 1\n[types.production]\nname = \"Not really\"\n",
    );
    let config = load(Some(&fx.repo()), &fx.user());
    assert!(
        config
            .problems
            .iter()
            .any(|p| p.file == path && p.message.contains("another name of Production"))
    );
}

#[test]
fn type_files_are_validated() {
    let fx = Fixture::new("validate");
    for (content, expected) in [
        ("[types.\"Bad Id\"]\n", "an id is lowercase"),
        ("[types.x]\ntone = \"purple\"\n", "unknown variant"),
        ("[types.x]\ncolor = \"#aa0000\"\n", "unknown field"),
        ("[types.x]\nname = \"  \"\n", "the name needs"),
        ("[types.x]\nclose_idle_after = 100000\n", "close_idle_after"),
        ("[types.x]\nauto_commit = false\n", "unknown field"),
        ("schema_version = 99\n", "newer version"),
    ] {
        let path = fx.write("user/connection-types.toml", content);
        let config = load(None, &fx.user());
        assert!(
            config
                .problems
                .iter()
                .any(|p| p.file == path && p.message.contains(expected)),
            "{content}: {:?}",
            config.problems
        );
    }
}

#[test]
fn personal_types_are_saved_keeping_the_rest_of_the_file() {
    let fx = Fixture::new("personal");
    let path = fx.user().join("connection-types.toml");
    let staging = ConnectionTypeConfig {
        name: Some(" Staging ".into()),
        tone: Some(Tone::Blue),
        description: Some("".into()),
        confirm_data_changes: true,
        close_idle_after: Some(0),
        ..Default::default()
    };
    set_personal_type(&fx.user(), "staging", &staging).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("# my types\n{content}")).unwrap();
    set_personal_type(
        &fx.user(),
        "dev",
        &ConnectionTypeConfig {
            tone: Some(Tone::Green),
            ..Default::default()
        },
    )
    .unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.starts_with("# my types\n"), "{content}");
    let config = load(None, &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let file = config.user_types.file().unwrap();
    let saved = &file.types["staging"];
    assert_eq!(saved.name.as_deref(), Some("Staging"));
    assert_eq!(saved.tone, Some(Tone::Blue));
    assert_eq!(saved.description, None);
    assert_eq!(saved.close_idle_after, None);
    assert!(saved.confirm_data_changes);

    // Invalid ones never reach the file.
    for (id, kind) in [
        ("Bad", ConnectionTypeConfig::default()),
        ("production", ConnectionTypeConfig::default()),
        (
            "x",
            ConnectionTypeConfig {
                name: Some("n".repeat(41)),
                ..Default::default()
            },
        ),
    ] {
        assert!(set_personal_type(&fx.user(), id, &kind).is_err(), "{id}");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content);

    remove_personal_type(&fx.user(), "staging").unwrap();
    let config = load(None, &fx.user());
    assert!(
        !config
            .user_types
            .file()
            .unwrap()
            .types
            .contains_key("staging")
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("# my types\n")
    );
}

#[test]
fn check_warns_about_agents_writing_where_a_type_confirms_data_changes() {
    let fx = Fixture::new("check");
    fx.write(
        "repo/.spelaion/connection-types.toml",
        "schema_version = 1\n[types.critical]\nname = \"Critical\"\nconfirm_data_changes = true\n",
    );
    let writes = "\n[mcp]\nexposed = true\nwrites = \"allow\"\n";
    fx.write(
        "repo/.spelaion/connections/a.toml",
        &format!("{}{writes}", sqlite("critical")),
    );
    fx.write(
        "repo/.spelaion/connections/b.toml",
        &format!("{}{writes}", sqlite("Production")),
    );
    fx.write(
        "repo/.spelaion/connections/c.toml",
        &format!("{}{writes}", sqlite("staging")),
    );
    let findings = check_workspace(&fx.repo());
    let warned = |file: &str| {
        findings.iter().any(|f| {
            f.severity == Severity::Warning
                && f.path.ends_with(file)
                && f.message.contains("lets AI agents write")
        })
    };
    assert!(warned("a.toml"), "{findings:?}");
    assert!(warned("b.toml"), "{findings:?}");
    assert!(!warned("c.toml"), "{findings:?}");
}

/// A personal type that asks before every run and before data changes.
const PAYMENTS: &str = "schema_version = 1\n[types.payments]\nname = \"Payments\"\nconfirm_execution = true\nconfirm_data_changes = true\n";

#[test]
fn a_duplicated_connection_keeps_its_type_and_safeguards() {
    let fx = Fixture::new("duplicate");
    fx.write("user/connection-types.toml", PAYMENTS);
    fx.write("user/connections/ledger.toml", &sqlite("payments"));
    fx.write(
        "repo/.spelaion/connections/orders.toml",
        &sqlite("Production"),
    );
    let mut trust = fx.trusted();
    for id in [personal("ledger"), ws("orders")] {
        let config = load(Some(&fx.repo()), &fx.user());
        let copy = duplicate_connection(&config, &mut trust, &id, &NoSecrets).unwrap();
        let original = fx.resolve(&trust, &id).unwrap();
        let copied = fx.resolve(&trust, &copy.id).unwrap();
        assert_eq!(copied.env, original.env, "{id}");
        assert_eq!(copied.kind, original.kind, "{id}");
        assert_eq!(copied.safeguards(), original.safeguards(), "{id}");
    }
    let config = load(Some(&fx.repo()), &fx.user());
    let copy = config
        .connections
        .iter()
        .find(|c| c.id.path == "orders-copy")
        .unwrap();
    assert!(fx.resolve(&trust, &copy.id).unwrap().is_production());
}

#[test]
fn a_pasted_connection_takes_its_type_from_here_and_cannot_bring_one() {
    let fx = Fixture::new("paste");
    fx.write("user/connection-types.toml", PAYMENTS);
    fx.write("user/connections/ledger.toml", &sqlite("payments"));
    fx.write("user/connections/live.toml", &sqlite("PRODUCTION"));
    let trust = TrustStore::load(&fx.user()).unwrap();

    // Ctrl+C carries the label as written; Ctrl+V resolves it here, with every safeguard.
    for (id, check) in [
        ("ledger", "env = \"payments\""),
        ("live", "env = \"PRODUCTION\""),
    ] {
        let config = load(None, &fx.user());
        let text = connection_clipboard_text(&config, &trust, &personal(id), &NoSecrets)
            .unwrap()
            .text;
        assert!(text.contains(check), "{text}");
        let pasted = paste_connection(&config, &text, &NoSecrets).unwrap();
        let original = fx.resolve(&trust, &personal(id)).unwrap();
        let copy = fx.resolve(&trust, &pasted.id).unwrap();
        assert_eq!(copy.kind, original.kind, "{id}");
        assert_eq!(copy.safeguards(), original.safeguards(), "{id}");
    }

    // Text from elsewhere cannot define or loosen a type: a connection file has no such keys,
    // and a type file is not a connection.
    let config = load(None, &fx.user());
    for text in [
        "schema_version = 1\nengine = \"sqlite\"\nenv = \"prod\"\ndatabase = \"x.db\"\nconfirm_data_changes = false\n",
        "schema_version = 1\nengine = \"sqlite\"\nenv = \"prod\"\ndatabase = \"x.db\"\n\n[types.prod]\nconfirm_data_changes = false\n",
        "schema_version = 1\nengine = \"sqlite\"\nenv = \"payments\"\ndatabase = \"x.db\"\n\n[kind]\nconfirm_execution = false\n",
        PAYMENTS,
    ] {
        assert!(
            paste_connection(&config, text, &NoSecrets).is_err(),
            "{text}"
        );
    }
    let before = std::fs::read_to_string(fx.user().join("connection-types.toml")).unwrap();
    // A label that names no type here stays a plain label: never a type from the text.
    let pasted = paste_connection(
        &config,
        "schema_version = 1\nname = \"Elsewhere\"\nengine = \"sqlite\"\nenv = \"critical\"\ndatabase = \"x.db\"\n",
        &NoSecrets,
    )
    .unwrap();
    assert_eq!(fx.resolve(&trust, &pasted.id).unwrap().kind, None);
    assert_eq!(
        std::fs::read_to_string(fx.user().join("connection-types.toml")).unwrap(),
        before,
        "a paste never writes a type"
    );
    // And if the user defines it later, its safeguards apply to the pasted connection too.
    set_personal_type(
        &fx.user(),
        "critical",
        &ConnectionTypeConfig {
            confirm_data_changes: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        fx.resolve(&trust, &pasted.id)
            .unwrap()
            .safeguards()
            .confirm_data_changes
    );
}

/// The form for a personal SQLite connection with this label.
fn sqlite_draft(fx: &Fixture, id: &str) -> ConnectionDraft {
    let config = load(None, &fx.user());
    ConnectionDraft::from_entry(config.connection(&personal(id)).unwrap())
}

#[test]
fn changing_the_engine_keeps_the_type_and_the_name_pattern_shows_its_id() {
    let fx = Fixture::new("engine-pattern");
    fx.write("user/connection-types.toml", PAYMENTS);
    fx.write("user/connections/ledger.toml", &sqlite("payments"));
    fx.write("user/connections/live.toml", &sqlite("Production"));

    let draft = sqlite_draft(&fx, "ledger");
    for engine in [Engine::Postgres, Engine::Duckdb, Engine::Mysql] {
        let (changed, report) = draft.with_engine(engine);
        assert_eq!(changed.env, "payments", "{engine:?}");
        assert!(
            report.kept.iter().any(|k| k == "env"),
            "{engine:?}: {:?}",
            report.kept
        );
    }

    // `{env}` is the label as chosen: the type's id from the picker, or what was typed.
    let pattern = NamePattern::parse("{database} ({env})").unwrap();
    let mut prod = sqlite_draft(&fx, "live");
    prod.database = "orders.db".into();
    assert_eq!(
        pattern.render(&NameValues::of_draft(&prod, "SQLite")),
        "orders (Production)"
    );
    prod.env = "prod".into();
    assert_eq!(
        pattern.render(&NameValues::of_draft(&prod, "SQLite")),
        "orders (prod)"
    );
}

#[test]
fn commit_safeguards_only_add_up_and_production_has_smart_commit() {
    let fx = Fixture::new("commit");
    // The team tries to switch Production's smart commit off; the user tries on a team type
    // that asks for it. Writing `false` is the same as not writing it (ADR-031).
    fx.write(
        "repo/.spelaion/connection-types.toml",
        "schema_version = 1\n[types.prod]\nsmart_commit = false\n[types.ledger]\nsmart_commit = true\nmanual_commit = true\n",
    );
    fx.write(
        "user/connection-types.toml",
        "schema_version = 1\n[types.ledger]\nsmart_commit = false\nmanual_commit = false\n[types.dev]\nmanual_commit = true\n",
    );
    fx.write("repo/.spelaion/connections/p.toml", &sqlite("prod"));
    fx.write("repo/.spelaion/connections/l.toml", &sqlite("ledger"));
    fx.write("repo/.spelaion/connections/d.toml", &sqlite("dev"));
    fx.write("repo/.spelaion/connections/t.toml", &sqlite("test"));
    let trust = fx.trusted();
    let prod = fx.resolve(&trust, &ws("p")).unwrap().safeguards();
    assert!(prod.smart_commit && !prod.manual_commit);
    let ledger = fx.resolve(&trust, &ws("l")).unwrap().safeguards();
    assert!(ledger.smart_commit && ledger.manual_commit);
    // The user may tighten a built-in type for themselves.
    let dev = fx.resolve(&trust, &ws("d")).unwrap().safeguards();
    assert!(dev.manual_commit && !dev.smart_commit);
    let test = fx.resolve(&trust, &ws("t")).unwrap().safeguards();
    assert!(!test.manual_commit && !test.smart_commit);
}

#[test]
fn switching_a_team_commit_safeguard_off_or_on_is_reviewed() {
    let fx = Fixture::new("commit-review");
    let with = "schema_version = 1\n[types.ledger]\nname = \"Ledger\"\nsmart_commit = true\n";
    let team = fx.write("repo/.spelaion/connection-types.toml", with);
    fx.write("repo/.spelaion/connections/l.toml", &sqlite("ledger"));
    let trust = fx.trusted();
    assert!(
        fx.resolve(&trust, &ws("l"))
            .unwrap()
            .safeguards()
            .smart_commit
    );
    for (what, edited) in [
        (
            "smart commit switched off",
            with.replace("smart_commit = true", ""),
        ),
        (
            "smart commit written false",
            with.replace("smart_commit = true", "smart_commit = false"),
        ),
        (
            "manual commit added",
            with.replace(
                "smart_commit = true",
                "smart_commit = true\nmanual_commit = true",
            ),
        ),
    ] {
        std::fs::write(&team, &edited).unwrap();
        let change = changed(fx.resolve(&trust, &ws("l")));
        assert!(
            change.fields.iter().any(|f| f.field.ends_with("_commit")),
            "{what}: {:?}",
            change.fields
        );
    }
    // A type trusted before these settings existed (none on) is not a change.
    std::fs::write(
        &team,
        "schema_version = 1\n[types.ledger]\nname = \"Ledger\"\n",
    )
    .unwrap();
    let trust = fx.trusted();
    std::fs::write(
        &team,
        "schema_version = 1\n[types.ledger]\nname = \"Ledger\"\nsmart_commit = false\nmanual_commit = false\n",
    )
    .unwrap();
    fx.resolve(&trust, &ws("l")).unwrap();
}
