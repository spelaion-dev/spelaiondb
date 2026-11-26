//! An SSH key's remembered passphrase (ADR-090), with temporary folders and a fake keychain
//! (the real one is never touched): one entry per key file, named after its canonical path;
//! nothing without a keychain; forgotten when a hop stops using the file and no other hop
//! does.

use crate::common;

use common::{FakeBackend, Fixture};
use spelaion_config::{
    Source, TunnelDraft, TunnelPasswords, forget_key_passphrase, hop_key_path,
    key_passphrase_account, key_passphrase_remembered, load, remember_key_passphrase,
    remembered_key_passphrase, update_tunnel,
};
use spelaion_driver_api::Secret;

fn tunnel(key: &str) -> String {
    format!(
        "schema_version = 1\n\n[[hops]]\nhost = \"bastion\"\nuser = \"ops\"\nauth = [\"key\"]\nidentity_file = \"{key}\"\n"
    )
}

#[test]
fn a_passphrase_is_kept_under_its_key_files_canonical_path() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("key-passphrase-name");
    let key = fx.write("user/keys/id_ed25519.ppk", "not read here");
    let backend = FakeBackend::default();
    assert_eq!(remembered_key_passphrase(&backend, &key), None);
    assert!(!key_passphrase_remembered(&backend, &key));

    remember_key_passphrase(&backend, &key, &Secret::new("the passphrase".to_string())).unwrap();
    let account = key_passphrase_account(&key);
    let canonical = std::fs::canonicalize(&key).unwrap();
    assert_eq!(account, format!("key:{}", canonical.display()));
    // Another spelling of the same file names the same entry.
    let other_spelling = fx
        .user()
        .join("keys")
        .join("..")
        .join("keys")
        .join("id_ed25519.ppk");
    assert_eq!(key_passphrase_account(&other_spelling), account);
    assert_eq!(
        remembered_key_passphrase(&backend, &other_spelling).map(|s| s.expose().to_string()),
        Some("the passphrase".to_string())
    );
    assert!(key_passphrase_remembered(&backend, &key));

    forget_key_passphrase(&backend, &key).unwrap();
    assert_eq!(remembered_key_passphrase(&backend, &key), None);
    // Forgetting what is not there is not an error.
    forget_key_passphrase(&backend, &key).unwrap();
}

#[test]
fn without_a_keychain_nothing_is_read_or_kept() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("key-passphrase-none");
    let key = fx.write("user/keys/id_rsa", "x");
    let backend = FakeBackend {
        no_keychain: true,
        ..FakeBackend::default()
    };
    assert!(remember_key_passphrase(&backend, &key, &Secret::new("p".to_string())).is_err());
    assert_eq!(remembered_key_passphrase(&backend, &key), None);
    assert!(!key_passphrase_remembered(&backend, &key));
    forget_key_passphrase(&backend, &key).unwrap();
    // A keychain that keeps nothing is an error, never a passphrase said to be saved.
    let forgetful = FakeBackend {
        fail_set: true,
        ..FakeBackend::default()
    };
    assert!(remember_key_passphrase(&forgetful, &key, &Secret::new("p".to_string())).is_err());
}

#[test]
fn a_hop_that_changes_its_key_forgets_the_old_ones_passphrase_unless_another_uses_it() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("key-passphrase-change");
    let old_key = fx.write("user/keys/old.ppk", "x");
    let new_key = fx.write("user/keys/new.ppk", "x");
    fx.write("user/tunnels/one.toml", &tunnel("keys/old.ppk"));
    fx.write("user/tunnels/two.toml", &tunnel("keys/old.ppk"));
    let backend = FakeBackend::default();
    remember_key_passphrase(&backend, &old_key, &Secret::new("old".to_string())).unwrap();

    let config = load(None, &fx.user());
    assert_eq!(
        hop_key_path(&config, Source::User, "keys/old.ppk").map(|p| key_passphrase_account(&p)),
        Some(key_passphrase_account(&old_key))
    );
    assert_eq!(hop_key_path(&config, Source::User, "${user.key}"), None);
    assert_eq!(hop_key_path(&config, Source::User, " "), None);

    // `one` moves to the new key, but `two` still uses the old one: it stays remembered.
    let entry = config.tunnel_for(Source::User, "one").unwrap();
    let mut draft = TunnelDraft::from_entry(entry);
    draft.hops[0].identity_file = "keys/new.ppk".into();
    update_tunnel(
        &config,
        Source::User,
        "one",
        &draft,
        &TunnelPasswords::default(),
        &backend,
    )
    .unwrap();
    assert!(key_passphrase_remembered(&backend, &old_key));

    // `two` moves too: nothing uses the old key any more, so its passphrase is forgotten.
    let config = load(None, &fx.user());
    let entry = config.tunnel_for(Source::User, "two").unwrap();
    let mut draft = TunnelDraft::from_entry(entry);
    draft.hops[0].identity_file = new_key.display().to_string();
    update_tunnel(
        &config,
        Source::User,
        "two",
        &draft,
        &TunnelPasswords::default(),
        &backend,
    )
    .unwrap();
    assert!(!key_passphrase_remembered(&backend, &old_key));
    assert!(
        backend
            .keychain
            .lock()
            .unwrap()
            .keys()
            .all(|k| !k.starts_with("key:")),
        "nothing else was written"
    );
}
