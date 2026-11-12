//! Changes to the trust store made at the same time (SDB-276). Every change saves the whole
//! store, so one that loaded it before another saved would write the other one away. The
//! desktop app hit it: the user trusted the repository and, while that was saving, moved a
//! connection; the move saved the store it had loaded, untrusted, and the trust was gone.
//! `TrustStore::update` makes each change start from what the earlier ones saved.

mod common;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{FakeBackend, Fixture, team};
use spelaion_config::{ConfigError, Resolver, TrustStore, connection_item, move_connection};

const SHOP: &str =
    "schema_version = 1\nname = \"Shop\"\nengine = \"sqlite\"\ndatabase = \"shop.db\"\n";

/// Starts `change` on another thread and returns once it has loaded the store; it saves a
/// moment later.
fn slow_update(
    user: std::path::PathBuf,
    change: impl FnOnce(&mut TrustStore) -> Result<(), ConfigError> + Send + 'static,
) -> thread::JoinHandle<()> {
    let (loaded, has_loaded) = mpsc::channel();
    let handle = thread::spawn(move || {
        TrustStore::update(&user, |trust| {
            loaded.send(()).unwrap();
            thread::sleep(Duration::from_millis(300));
            change(trust)
        })
        .unwrap();
    });
    has_loaded.recv().unwrap();
    handle
}

#[test]
fn a_move_while_the_repository_is_being_trusted_keeps_the_trust() {
    let fx = Fixture::new("trust-update-move");
    fx.write("repo/.spelaion/connections/shop.toml", SHOP);
    let config = fx.config();
    let backend = FakeBackend::default();

    let trusting = slow_update(fx.user(), {
        let config = config.clone();
        move |trust| trust.trust_if_untrusted(&config)
    });
    // The explorer's drag, while the trust is still saving.
    let moved = TrustStore::update(&fx.user(), |trust| {
        move_connection(&config, trust, &team("shop"), "docker", &backend)
    })
    .unwrap();
    trusting.join().unwrap();

    let trust = TrustStore::load(&fx.user()).unwrap();
    assert!(trust.is_trusted(&fx.repo()), "the move kept the trust");
    // The moved connection keeps its pin: it resolves with no change to review.
    let resolved = Resolver::new(&fx.config(), &trust, &backend).resolve(&moved.id);
    assert!(resolved.is_ok(), "{:?}", resolved.err());
}

#[test]
fn a_revoke_while_another_change_is_saving_stays() {
    let fx = Fixture::new("trust-update-revoke");
    fx.write("repo/.spelaion/connections/shop.toml", SHOP);
    let config = fx.config();
    TrustStore::update(&fx.user(), |trust| trust.trust(&config)).unwrap();

    // Another change to the repository is saving (it loaded the store while still trusted)...
    let accepting = slow_update(fx.user(), {
        let config = config.clone();
        move |trust| trust.accept(&config, &connection_item(&team("shop")))
    });
    // ...when the user revokes the trust.
    TrustStore::update(&fx.user(), |trust| trust.revoke(&fx.repo())).unwrap();
    accepting.join().unwrap();

    let trust = TrustStore::load(&fx.user()).unwrap();
    assert!(!trust.is_trusted(&fx.repo()), "the revoke stays");
}
