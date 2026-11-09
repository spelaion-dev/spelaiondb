//! Moving connections between folders, with temporary directories and a fake secret backend
//! (the real keychain is never touched). The repository cases try to get around trust
//! (ADR-022): a move must never make a changed or new connection usable.

mod common;

use common::{FakeBackend, Fixture, personal, team};
use spelaion_config::{
    ConnectionId, Password, ResolveError, Resolver, TrustStore, connection_account, move_connection,
};

const ORDERS: &str = r#"#:schema https://spelaion.dev/schema/db/connection.schema.json

# Orders, as the team uses it.
schema_version = 1
name = "Orders" # shown in the explorer
engine = "postgres"
host = "orders.internal"
database = "orders"
user = "app"
password = { secret = "keychain" }
"#;

const LOCAL: &str = "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"local.db\"\n";

fn resolve(fx: &Fixture, backend: &FakeBackend, id: &ConnectionId) -> Result<(), ResolveError> {
    let config = fx.config();
    let trust = TrustStore::load(&fx.user()).unwrap();
    Resolver::new(&config, &trust, backend)
        .resolve(id)
        .map(|_| ())
}

fn changed(result: Result<(), ResolveError>) -> spelaion_config::ItemChange {
    match result {
        Err(ResolveError::Changed { change, .. }) => change,
        other => panic!("expected a change to review, got {other:?}"),
    }
}

#[test]
fn a_personal_connection_moves_between_nested_folders_with_its_password() {
    let fx = Fixture::new("move-personal");
    fx.write("user/connections/prod/eu/orders.toml", ORDERS);
    let backend = FakeBackend::default();
    let config = fx.config();
    let old = connection_account(&config, &personal("prod/eu/orders"));
    backend
        .keychain
        .lock()
        .unwrap()
        .insert(old.clone(), "s3cret".into());
    // An entry left behind by an older connection of the new name.
    let stale = connection_account(&config, &personal("staging/orders"));
    backend
        .keychain
        .lock()
        .unwrap()
        .insert(stale.clone(), "old".into());
    let mut trust = fx.untrusted();

    let moved = move_connection(
        &config,
        &mut trust,
        &personal("prod/eu/orders"),
        "Staging",
        &backend,
    )
    .unwrap();
    assert_eq!(moved.id, personal("staging/orders"));
    assert!(moved.password_moved);
    // Byte for byte: comments, order and hand-written keys stay.
    assert_eq!(fx.read("user/connections/staging/orders.toml"), ORDERS);
    assert!(
        !fx.user().join("connections/prod").exists(),
        "empty folders go"
    );
    let keychain = backend.keychain.lock().unwrap().clone();
    assert_eq!(keychain[&stale], "s3cret", "the stale entry is replaced");
    assert!(!keychain.contains_key(&old), "the old entry leaves");

    // It keeps working: the password is found under its new identifier.
    let config = fx.config();
    let trust = fx.untrusted();
    let resolved = Resolver::new(&config, &trust, &backend)
        .resolve(&moved.id)
        .unwrap();
    assert!(matches!(resolved.password, Password::Value(ref s) if s.expose() == "s3cret"));

    // Into a deeper folder, then back out to no folder.
    let mut trust = fx.untrusted();
    let deeper = move_connection(
        &config,
        &mut trust,
        &moved.id,
        "/staging/eu/west/",
        &backend,
    )
    .unwrap();
    assert_eq!(deeper.id, personal("staging/eu/west/orders"));
    let out = move_connection(&fx.config(), &mut trust, &deeper.id, "", &backend).unwrap();
    assert_eq!(out.id, personal("orders"));
    assert_eq!(fx.read("user/connections/orders.toml"), ORDERS);
    assert!(!fx.user().join("connections/staging").exists());
    let keychain = backend.keychain.lock().unwrap().clone();
    assert_eq!(keychain.len(), 1);
    assert_eq!(
        keychain[&connection_account(&fx.config(), &out.id)],
        "s3cret"
    );

    // Its own folder: nothing to do.
    let same = move_connection(&fx.config(), &mut trust, &out.id, "", &backend).unwrap();
    assert_eq!(same.id, out.id);
    assert!(!same.password_moved);
}

#[test]
fn a_move_never_replaces_a_file_nor_moves_one_changed_on_disk() {
    let fx = Fixture::new("move-refused");
    fx.write("user/connections/orders.toml", ORDERS);
    fx.write("user/connections/prod/orders.toml", LOCAL);
    fx.write("user/connections/broken/orders.toml", "not toml");
    let backend = FakeBackend::default();
    let mut trust = fx.untrusted();

    let err = move_connection(
        &fx.config(),
        &mut trust,
        &personal("orders"),
        "prod",
        &backend,
    )
    .unwrap_err();
    assert!(err.to_string().contains("already has"), "{err}");
    // A file that does not load is not replaced either.
    let err = move_connection(
        &fx.config(),
        &mut trust,
        &personal("orders"),
        "broken",
        &backend,
    )
    .unwrap_err();
    assert!(err.to_string().contains("already has"), "{err}");
    assert_eq!(fx.read("user/connections/prod/orders.toml"), LOCAL);
    assert_eq!(fx.read("user/connections/broken/orders.toml"), "not toml");
    assert_eq!(fx.read("user/connections/orders.toml"), ORDERS);

    // Folders that would leave the connections directory, or have no name, are refused.
    for folder in ["..", "a/../..", "../../escape", "?", "eu/./"] {
        let err = move_connection(
            &fx.config(),
            &mut trust,
            &personal("orders"),
            folder,
            &backend,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("invalid folder"),
            "{folder}: {err}"
        );
    }
    assert!(!fx.base.join("escape").exists());

    // Changed on disk since it was loaded: refresh first.
    let config = fx.config();
    fx.write(
        "user/connections/orders.toml",
        &ORDERS.replace("orders.internal", "elsewhere"),
    );
    let err =
        move_connection(&config, &mut trust, &personal("orders"), "new", &backend).unwrap_err();
    assert!(err.to_string().contains("changed"), "{err}");
    assert!(!fx.user().join("connections/new").exists());

    // A keychain that refuses: nothing moves.
    let config = fx.config();
    backend.keychain.lock().unwrap().insert(
        connection_account(&config, &personal("orders")),
        "s3cret".into(),
    );
    let locked = FakeBackend {
        fail_set: true,
        ..FakeBackend::default()
    };
    locked
        .keychain
        .lock()
        .unwrap()
        .extend(backend.keychain.lock().unwrap().clone());
    let err =
        move_connection(&config, &mut trust, &personal("orders"), "new", &locked).unwrap_err();
    assert!(matches!(err, ResolveError::Secret { .. }), "{err}");
    assert!(fx.user().join("connections/orders.toml").exists());
    assert!(!fx.user().join("connections/new").exists());
}

#[test]
fn a_moved_repository_connection_as_trusted_stays_trusted() {
    let fx = Fixture::new("move-trusted");
    fx.write("repo/.spelaion/connections/eu/orders.toml", ORDERS);
    fx.write("repo/.spelaion/connections/local.toml", LOCAL);
    let backend = FakeBackend::default();
    let config = fx.config();
    backend.keychain.lock().unwrap().insert(
        connection_account(&config, &team("eu/orders")),
        "s3cret".into(),
    );
    let mut trust = fx.trusted();
    resolve(&fx, &backend, &team("eu/orders")).unwrap();

    let moved =
        move_connection(&config, &mut trust, &team("eu/orders"), "prod/eu", &backend).unwrap();
    assert_eq!(moved.id, team("prod/eu/orders"));
    assert_eq!(
        fx.read("repo/.spelaion/connections/prod/eu/orders.toml"),
        ORDERS
    );
    assert!(!fx.repo().join(".spelaion/connections/eu").exists());
    let reloaded = TrustStore::load(&fx.user()).unwrap();
    assert!(reloaded.changes(&fx.config()).is_empty());
    resolve(&fx, &backend, &moved.id).unwrap();

    // Back to the top, and a connection without a password: still trusted.
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    move_connection(&fx.config(), &mut trust, &moved.id, "", &backend).unwrap();
    move_connection(&fx.config(), &mut trust, &team("local"), "dev", &backend).unwrap();
    resolve(&fx, &backend, &team("orders")).unwrap();
    resolve(&fx, &backend, &team("dev/local")).unwrap();
    assert!(
        TrustStore::load(&fx.user())
            .unwrap()
            .changes(&fx.config())
            .is_empty()
    );

    // Edited by hand after the move: refused until reviewed, like any file.
    fx.write(
        "repo/.spelaion/connections/orders.toml",
        &ORDERS.replace("orders.internal", "attacker.example.com"),
    );
    let change = changed(resolve(&fx, &backend, &team("orders")));
    assert!(!change.new);
    assert_eq!(change.fields[0].field, "host");
}

#[test]
fn a_move_never_makes_a_changed_or_new_repository_connection_usable() {
    let fx = Fixture::new("move-untrusted-change");
    fx.write("repo/.spelaion/connections/eu/orders.toml", ORDERS);
    fx.write("repo/.spelaion/connections/old/gone.toml", LOCAL);
    let backend = FakeBackend::default();
    fx.trusted();

    // A pull changes the host; moving it does not accept the change.
    let attacker = ORDERS.replace("orders.internal", "attacker.example.com");
    fx.write("repo/.spelaion/connections/eu/orders.toml", &attacker);
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    let moved = move_connection(
        &fx.config(),
        &mut trust,
        &team("eu/orders"),
        "prod",
        &backend,
    )
    .unwrap();
    let change = changed(resolve(&fx, &backend, &moved.id));
    assert!(!change.new, "the same difference to review, not a new item");
    assert_eq!(change.fields.len(), 1);
    assert_eq!(change.fields[0].field, "host");
    assert_eq!(change.fields[0].before.as_deref(), Some("orders.internal"));
    assert_eq!(
        change.fields[0].after.as_deref(),
        Some("attacker.example.com")
    );
    // Moving it back does not either.
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    move_connection(&fx.config(), &mut trust, &moved.id, "eu", &backend).unwrap();
    changed(resolve(&fx, &backend, &team("eu/orders")));

    // A file removed from the repository leaves its pin behind; a new, never reviewed
    // connection with the very same content moved onto its path must not inherit it.
    std::fs::remove_file(fx.repo().join(".spelaion/connections/old/gone.toml")).unwrap();
    fx.write("repo/.spelaion/connections/incoming/gone.toml", LOCAL);
    assert!(changed(resolve(&fx, &backend, &team("incoming/gone"))).new);
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    let moved = move_connection(
        &fx.config(),
        &mut trust,
        &team("incoming/gone"),
        "old",
        &backend,
    )
    .unwrap();
    assert_eq!(moved.id, team("old/gone"));
    assert!(changed(resolve(&fx, &backend, &moved.id)).new);
}

#[test]
fn a_trusted_pin_leaves_the_old_path_with_the_file() {
    let fx = Fixture::new("move-old-path");
    fx.write("repo/.spelaion/connections/eu/orders.toml", ORDERS);
    let backend = FakeBackend::default();
    let mut trust = fx.trusted();
    move_connection(&fx.config(), &mut trust, &team("eu/orders"), "us", &backend).unwrap();
    // Someone puts a file back at the old path (a pull, a checkout): it is reviewed as new,
    // even with the content the user once trusted there.
    fx.write(
        "repo/.spelaion/connections/eu/orders.toml",
        &ORDERS.replace("orders.internal", "attacker.example.com"),
    );
    assert!(changed(resolve(&fx, &backend, &team("eu/orders"))).new);
    fx.write("repo/.spelaion/connections/eu/orders.toml", ORDERS);
    assert!(changed(resolve(&fx, &backend, &team("eu/orders"))).new);
    resolve(&fx, &backend, &team("us/orders")).unwrap();
}

#[test]
fn an_untrusted_repository_connection_moves_but_stays_untrusted() {
    let fx = Fixture::new("move-untrusted");
    fx.write("repo/.spelaion/connections/eu/orders.toml", ORDERS);
    let backend = FakeBackend::default();
    let mut trust = fx.untrusted();
    let moved = move_connection(
        &fx.config(),
        &mut trust,
        &team("eu/orders"),
        "prod",
        &backend,
    )
    .unwrap();
    assert_eq!(moved.id, team("prod/orders"));
    let err = resolve(&fx, &backend, &moved.id).unwrap_err();
    assert!(matches!(err, ResolveError::Untrusted { .. }), "{err}");
    // Trusting it afterwards is what pins it, as with any repository.
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    assert!(!trust.is_trusted(&fx.repo()));
    trust.trust(&fx.config()).unwrap();
    resolve(&fx, &backend, &moved.id).unwrap();
}
