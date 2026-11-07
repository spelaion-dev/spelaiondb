//! A connection's description and tags survive what ADR-030 does to connections:
//! duplicating, copying and pasting, and changing its engine. Temporary directories and a fake secret backend (the
//! real keychain is never touched).

mod common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{
    ConnectionDraft, Engine, connection_clipboard_text, duplicate_connection, paste_connection,
};

const DESCRIBED: &str = "schema_version = 1\nname = \"Orders\"\ndescription = \"\"\"\nOrders of the EU shop.\nAsk #data first.\"\"\"\nengine = \"postgres\"\nhost = \"orders.internal\"\ndatabase = \"orders\"\nuser = \"app\"\n";
const DESCRIPTION: &str = "Orders of the EU shop.\nAsk #data first.";

#[test]
fn descriptions_survive_duplicates_copies_and_engine_changes() {
    let fx = Fixture::new("descriptive-keys");
    fx.write("user/connections/orders.toml", DESCRIBED);
    let config = fx.config();
    let backend = FakeBackend::default();
    let mut trust = fx.trusted();

    // Duplicating keeps every key.
    let duplicated =
        duplicate_connection(&config, &mut trust, &personal("orders"), &backend).unwrap();
    assert_eq!(duplicated.file.description.as_deref(), Some(DESCRIPTION));
    let config = fx.config();
    assert_eq!(
        config
            .connection(&duplicated.id)
            .unwrap()
            .file
            .description
            .as_deref(),
        Some(DESCRIPTION)
    );

    // The clipboard text carries it, and pasting it back keeps it.
    let copied = connection_clipboard_text(&config, &trust, &personal("orders"), &backend).unwrap();
    assert!(copied.text.contains("Ask #data first."), "{}", copied.text);
    let pasted = paste_connection(&config, &copied.text, &backend).unwrap();
    let config = fx.config();
    assert_eq!(
        config
            .connection(&pasted.id)
            .unwrap()
            .file
            .description
            .as_deref(),
        Some(DESCRIPTION)
    );

    // Pasted text is checked like the form: no hidden characters, no endless text.
    let hidden = copied.text.replace("Ask #data", "Ask \u{202e}#data");
    let error = paste_connection(&config, &hidden, &backend)
        .unwrap_err()
        .to_string();
    assert!(error.contains("only hold text"), "{error}");
    let long = copied.text.replace("Ask #data first.", &"x".repeat(2_001));
    let error = paste_connection(&config, &long, &backend)
        .unwrap_err()
        .to_string();
    assert!(error.contains("at most 2000"), "{error}");

    // An engine change keeps it, and says so.
    let draft = ConnectionDraft::from_entry(config.connection(&personal("orders")).unwrap());
    for engine in [Engine::Mysql, Engine::Sqlserver, Engine::Sqlite] {
        let (next, report) = draft.with_engine(engine);
        assert_eq!(next.description, DESCRIPTION, "{engine:?}");
        assert!(
            report.kept.iter().any(|k| k == "description"),
            "{engine:?}: {report:?}"
        );
    }
}

const TAGGED: &str = "schema_version = 1\nname = \"Billing\"\ntags = [\"billing\", \"eu\"]\nengine = \"mysql\"\nhost = \"billing.internal\"\nuser = \"app\"\n";

fn tags_of(fx: &Fixture, id: &spelaion_config::ConnectionId) -> Vec<String> {
    fx.config().connection(id).unwrap().file.tags.clone()
}

#[test]
fn tags_survive_duplicates_copies_and_engine_changes() {
    let fx = Fixture::new("tagged-copies");
    fx.write("user/connections/billing.toml", TAGGED);
    let config = fx.config();
    let backend = FakeBackend::default();
    let mut trust = fx.trusted();

    // Duplicating keeps them.
    let duplicated =
        duplicate_connection(&config, &mut trust, &personal("billing"), &backend).unwrap();
    assert_eq!(duplicated.file.tags, ["billing", "eu"]);
    assert_eq!(tags_of(&fx, &duplicated.id), ["billing", "eu"]);

    // The clipboard text carries them, and pasting it back keeps them.
    let config = fx.config();
    let copied =
        connection_clipboard_text(&config, &trust, &personal("billing"), &backend).unwrap();
    assert!(copied.text.contains("\"billing\""), "{}", copied.text);
    let pasted = paste_connection(&config, &copied.text, &backend).unwrap();
    assert_eq!(tags_of(&fx, &pasted.id), ["billing", "eu"]);

    // Pasted tags go through the form's rules: cleaned up, then validated.
    let messy = TAGGED.replace(
        "[\"billing\", \"eu\"]",
        "[\" billing \", \"EU\", \"eu\", \"\", \"team  data\"]",
    );
    let config = fx.config();
    let pasted = paste_connection(&config, &messy, &backend).unwrap();
    assert_eq!(tags_of(&fx, &pasted.id), ["billing", "EU", "team data"]);
    let config = fx.config();
    for (bad, message) in [
        ("[\"a,b\"]", "without commas"),
        ("[\"rtl\\u202e\"]", "without commas"),
        (
            "[\"far too long for a tag of a connection\"]",
            "longer than 32",
        ),
    ] {
        let text = TAGGED.replace("[\"billing\", \"eu\"]", bad);
        let error = paste_connection(&config, &text, &backend)
            .unwrap_err()
            .to_string();
        assert!(error.contains(message), "{bad}: {error}");
    }
    let many: Vec<String> = (0..21).map(|i| format!("\"t{i}\"")).collect();
    let text = TAGGED.replace("[\"billing\", \"eu\"]", &format!("[{}]", many.join(", ")));
    let error = paste_connection(&config, &text, &backend)
        .unwrap_err()
        .to_string();
    assert!(error.contains("at most 20 tags"), "{error}");

    // An engine change keeps them, and says so.
    let draft = ConnectionDraft::from_entry(config.connection(&personal("billing")).unwrap());
    for engine in [Engine::Postgres, Engine::Mariadb, Engine::Duckdb] {
        let (next, report) = draft.with_engine(engine);
        assert_eq!(next.tags, ["billing", "eu"], "{engine:?}");
        assert!(
            report.kept.iter().any(|k| k == "tags"),
            "{engine:?}: {report:?}"
        );
    }
}

/// The connection type (`env`, ADR-029), the description and the tags together.
const DESCRIBED_TYPED_TAGGED: &str = "schema_version = 1\nname = \"Ledger\"\ndescription = \"The finance ledger.\"\ntags = [\"finance\", \"eu\"]\nengine = \"postgres\"\nenv = \"prod\"\nhost = \"ledger.internal\"\ndatabase = \"ledger\"\nuser = \"app\"\n";

#[test]
fn the_type_description_and_tags_travel_together() {
    let fx = Fixture::new("typed-described-tagged");
    fx.write("user/connections/ledger.toml", DESCRIBED_TYPED_TAGGED);
    let config = fx.config();
    let backend = FakeBackend::default();
    let mut trust = fx.trusted();
    let all_three = |id: &spelaion_config::ConnectionId| {
        let config = fx.config();
        let file = config.connection(id).unwrap().file.clone();
        (file.env, file.description, file.tags)
    };
    let expected = (
        Some("prod".to_string()),
        Some("The finance ledger.".to_string()),
        vec!["finance".to_string(), "eu".to_string()],
    );

    let duplicated =
        duplicate_connection(&config, &mut trust, &personal("ledger"), &backend).unwrap();
    assert_eq!(all_three(&duplicated.id), expected);

    let copied = connection_clipboard_text(&config, &trust, &personal("ledger"), &backend).unwrap();
    let pasted = paste_connection(&fx.config(), &copied.text, &backend).unwrap();
    assert_eq!(all_three(&pasted.id), expected);

    let config = fx.config();
    let draft = ConnectionDraft::from_entry(config.connection(&personal("ledger")).unwrap());
    let (next, report) = draft.with_engine(Engine::Mysql);
    assert_eq!(
        (
            next.env.as_str(),
            next.description.as_str(),
            next.tags.clone()
        ),
        ("prod", "The finance ledger.", expected.2.clone())
    );
    for key in ["env", "description", "tags"] {
        assert!(report.kept.iter().any(|k| k == key), "{key}: {report:?}");
    }
}
