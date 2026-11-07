//! A connection's description survives what ADR-030 does to connections: duplicating, copying
//! and pasting, and changing its engine. Temporary directories and a fake secret backend (the
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
