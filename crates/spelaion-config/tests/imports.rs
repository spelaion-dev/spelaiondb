//! The import flow every client shares (`import.rs`): which connections already exist, what a
//! replacement keeps, where imported connections go, and the passwords another client keeps
//! in the keychain. Temporary directories and a fake secret backend: the real keychain is
//! never touched.

mod common;

use common::{FakeBackend, Fixture, personal, team};
use spelaion_config::{
    ConnectionDraft, Engine, ImportClient, ImportedConnection, ImportedSsh, ImportedTls,
    KeychainItem, PasswordChoice, Source, TargetFolder, TlsModeConfig, add_connection,
    connection_account, find_duplicate, import_names, import_plan, read_stored_passwords,
    replacement_draft, update_connection,
};
use spelaion_driver_api::Secret;

const ORDERS: &str = r#"schema_version = 1
name = "Orders"
engine = "postgres"
host = "db.internal"
database = "orders"
user = "app"
password = { secret = "keychain" }
read_only = true
statement_timeout = 30
description = "The team's orders database."

[mcp]
exposed = true
"#;

fn imported(name: &str) -> ImportedConnection {
    ImportedConnection {
        id: format!("id-{name}"),
        name: name.into(),
        description: String::new(),
        tags: Vec::new(),
        folder: String::new(),
        engine: Ok(Engine::Postgres),
        host: "db.internal".into(),
        port: Some(5432),
        database: "orders".into(),
        user: "app".into(),
        password: None,
        stored_password: None,
        env: String::new(),
        read_only: false,
        ssh: None,
        proxy: None,
        tls: None,
        unmapped: Vec::new(),
    }
}

fn top_level() -> TargetFolder {
    TargetFolder::default()
}

#[test]
fn duplicates_are_found_by_name_or_by_the_database_they_reach() {
    let fx = Fixture::new("import-dup");
    fx.write("user/connections/orders.toml", ORDERS);
    fx.write(
        "repo/.spelaion/connections/files/local.toml",
        "schema_version = 1\nname = \"Local\"\nengine = \"sqlite\"\ndatabase = \"C:\\\\data\\\\local.db\"\n",
    );
    let config = fx.config();

    // The same name, whatever the case, in the same place.
    let mut other = imported("ORDERS");
    other.host = "elsewhere".into();
    assert_eq!(
        find_duplicate(&config, &other, Source::User).map(|e| e.id.clone()),
        Some(personal("orders"))
    );
    // Another name that reaches the same database (the default port counts as 5432).
    let same = imported("Orders on prod");
    assert_eq!(
        find_duplicate(&config, &same, Source::User).map(|e| e.id.clone()),
        Some(personal("orders"))
    );
    // Only among the connections of where the import goes.
    assert!(find_duplicate(&config, &same, Source::Workspace).is_none());
    // Another user, database or engine is another connection.
    for change in [
        |c: &mut ImportedConnection| c.user = "reporting".into(),
        |c: &mut ImportedConnection| c.database = "orders_test".into(),
        |c: &mut ImportedConnection| c.engine = Ok(Engine::Mysql),
        |c: &mut ImportedConnection| c.port = Some(6543),
    ] {
        let mut different = imported("Something else");
        change(&mut different);
        assert!(find_duplicate(&config, &different, Source::User).is_none());
    }
    // A file is the same file whatever the slashes (and, on Windows, the case).
    let mut file = imported("Another name");
    file.engine = Ok(Engine::Sqlite);
    file.database = if cfg!(windows) {
        "c:/DATA/local.db".into()
    } else {
        "C:\\data\\local.db".into()
    };
    assert_eq!(
        find_duplicate(&config, &file, Source::Workspace).map(|e| e.id.clone()),
        Some(team("files/local"))
    );
    // A connection the engine cannot open duplicates nothing by its address.
    let mut oracle = imported("Legacy");
    oracle.engine = Err("oracle is not supported yet".into());
    assert!(find_duplicate(&config, &oracle, Source::User).is_none());
}

#[test]
fn a_copy_takes_the_clients_name_after_its_own() {
    let names: Vec<String> = import_names("Orders", ImportClient::Datagrip)
        .take(3)
        .collect();
    assert_eq!(
        names,
        ["Orders", "Orders (DataGrip)", "Orders (DataGrip 2)"]
    );
    assert_eq!(import_names("x", ImportClient::Dbeaver).count(), 10);
}

#[test]
fn the_target_folder_holds_the_clients_folders_or_replaces_them() {
    let keep = TargetFolder {
        folder: "Imported/DataGrip".into(),
        keep_client_folders: true,
    };
    assert_eq!(keep.for_connection("Prod/EU"), "imported/datagrip/prod/eu");
    assert_eq!(keep.for_connection(""), "imported/datagrip");
    let flat = TargetFolder {
        folder: " /Imported/ ".into(),
        keep_client_folders: false,
    };
    assert_eq!(flat.for_connection("Prod"), "imported");
    assert_eq!(top_level().for_connection("Prod"), "");
    let kept = TargetFolder {
        folder: String::new(),
        keep_client_folders: true,
    };
    assert_eq!(kept.for_connection("Prod"), "prod");
    // A folder that cannot be a folder is refused before anything is written.
    assert!(keep.check().is_ok());
    let bad = TargetFolder {
        folder: "..".into(),
        keep_client_folders: false,
    };
    assert!(bad.check().is_err());

    let plan = import_plan(
        &ImportedConnection {
            folder: "Prod".into(),
            ..imported("Orders")
        },
        Engine::Postgres,
        Source::User,
        false,
        "",
        &keep,
    );
    assert_eq!(plan.connection.folder, "imported/datagrip/prod");
}

#[test]
fn tls_and_read_only_come_with_the_connection_but_not_to_a_file() {
    let mut connection = imported("Orders");
    connection.read_only = true;
    connection.tls = Some(ImportedTls {
        mode: TlsModeConfig::VerifyCa,
        ca: "C:\\certs\\ca.crt".into(),
        cert: "C:\\certs\\client.crt".into(),
        key: "C:\\certs\\client.key".into(),
    });
    let plan = import_plan(
        &connection,
        Engine::Postgres,
        Source::User,
        true,
        "",
        &top_level(),
    );
    let draft = &plan.connection;
    assert!(draft.read_only);
    assert_eq!(draft.tls_mode, TlsModeConfig::VerifyCa);
    assert_eq!(
        (
            draft.tls_ca.as_str(),
            draft.tls_cert.as_str(),
            draft.tls_key.as_str()
        ),
        (
            "C:\\certs\\ca.crt",
            "C:\\certs\\client.crt",
            "C:\\certs\\client.key"
        )
    );
    let fx = Fixture::new("import-tls");
    let config = fx.config();
    let file = draft.to_file(&config).unwrap();
    assert_eq!(file.tls.mode, TlsModeConfig::VerifyCa);

    let mut sqlite = connection.clone();
    sqlite.engine = Ok(Engine::Sqlite);
    sqlite.database = "C:\\data\\x.db".into();
    let plan = import_plan(
        &sqlite,
        Engine::Sqlite,
        Source::User,
        true,
        "",
        &top_level(),
    );
    assert_eq!(plan.connection.tls_mode, TlsModeConfig::default());
    assert!(plan.connection.tls_ca.is_empty());
}

#[test]
fn replacing_takes_the_clients_settings_and_keeps_everything_else() {
    let fx = Fixture::new("import-replace");
    fx.write("user/connections/orders.toml", ORDERS);
    let config = fx.config();
    let backend = FakeBackend::default();
    let account = connection_account(&config, &personal("orders"));
    backend
        .keychain
        .lock()
        .unwrap()
        .insert(account.clone(), "old".into());

    let mut incoming = imported("Orders");
    incoming.host = "db2.internal".into();
    incoming.port = Some(6432);
    incoming.user = "app2".into();
    incoming.password = Some(Secret::new("from-the-client"));
    incoming.tls = Some(ImportedTls {
        mode: TlsModeConfig::Require,
        ca: String::new(),
        cert: String::new(),
        key: String::new(),
    });
    let existing = find_duplicate(&config, &incoming, Source::User).unwrap();
    let plan = import_plan(
        &incoming,
        Engine::Postgres,
        Source::User,
        true,
        "",
        &TargetFolder {
            folder: "imported".into(),
            keep_client_folders: false,
        },
    );
    let draft = replacement_draft(existing, &plan.connection);
    // The name, folder, description, timeouts and agents' access stay; read-only stays on.
    assert_eq!(draft.name, "Orders");
    assert_eq!(draft.folder, "");
    assert!(draft.read_only && draft.mcp_exposed);
    assert_eq!(draft.statement_timeout, Some(30));
    assert_eq!(draft.description, "The team's orders database.");
    assert_eq!(
        (draft.host.as_str(), draft.port, draft.user.as_str()),
        ("db2.internal", Some(6432), "app2")
    );
    assert_eq!(draft.password, PasswordChoice::Keychain);
    update_connection(
        &config,
        &personal("orders"),
        &draft,
        plan.password.as_ref(),
        &backend,
    )
    .unwrap();
    let text = fx.read("user/connections/orders.toml");
    assert!(text.contains("host = \"db2.internal\""), "{text}");
    assert!(text.contains("statement_timeout = 30"), "{text}");
    assert!(text.contains("[mcp]"), "{text}");
    assert!(
        !text.contains("from-the-client"),
        "a password reached the file: {text}"
    );
    // The client's password replaced the stored one, in the keychain only.
    assert_eq!(
        backend
            .keychain
            .lock()
            .unwrap()
            .get(&account)
            .map(String::as_str),
        Some("from-the-client")
    );
    // Still one connection: replaced, not added.
    assert_eq!(fx.config().connections.len(), 1);
}

#[test]
fn passwords_in_another_apps_keychain_are_read_only_when_asked_and_never_written_to_files() {
    let fx = Fixture::new("import-keychain");
    let config = fx.config();
    let backend = FakeBackend::default();
    backend.other_apps.lock().unwrap().insert(
        ("IntelliJ Platform DB — 1234".into(), "app".into()),
        "kept-by-datagrip".into(),
    );
    let mut connections = vec![
        ImportedConnection {
            stored_password: Some(KeychainItem {
                service: "IntelliJ Platform DB — 1234".into(),
                account: "app".into(),
            }),
            ..imported("Orders")
        },
        // Saved, but the entry is gone: asked when connecting.
        ImportedConnection {
            stored_password: Some(KeychainItem {
                service: "IntelliJ Platform DB — 5678".into(),
                account: "app".into(),
            }),
            ..imported("Gone")
        },
        imported("Nothing saved"),
    ];
    assert!(connections[0].has_password() && !connections[2].has_password());
    // Nothing is read before this call (the preview never reads the keychain).
    assert!(backend.other_app_reads.lock().unwrap().is_empty());
    let failed = read_stored_passwords(&mut connections, &backend);
    assert!(failed.is_empty());
    assert_eq!(backend.other_app_reads.lock().unwrap().len(), 2);
    assert_eq!(
        connections[0].password.as_ref().map(Secret::expose),
        Some("kept-by-datagrip")
    );
    assert!(connections[1].password.is_none() && connections[2].password.is_none());

    let plan = import_plan(
        &connections[0],
        Engine::Postgres,
        Source::User,
        true,
        "",
        &top_level(),
    );
    let id = add_connection(&config, &plan.connection, plan.password.as_ref(), &backend).unwrap();
    let account = connection_account(&config, &id);
    assert_eq!(
        backend
            .keychain
            .lock()
            .unwrap()
            .get(&account)
            .map(String::as_str),
        Some("kept-by-datagrip")
    );
    let text = fx.read("user/connections/orders.toml");
    assert!(!text.contains("kept-by-datagrip"), "{text}");
    let gone = import_plan(
        &connections[1],
        Engine::Postgres,
        Source::User,
        true,
        "",
        &top_level(),
    );
    assert_eq!(gone.connection.password, PasswordChoice::Prompt);

    // A keychain that refuses to read leaves the password out and says which connection.
    let locked = FakeBackend {
        fail_get: true,
        ..FakeBackend::default()
    };
    let mut again = vec![ImportedConnection {
        stored_password: Some(KeychainItem {
            service: "IntelliJ Platform DB — 1234".into(),
            account: "app".into(),
        }),
        ..imported("Orders")
    }];
    let failed = read_stored_passwords(&mut again, &locked);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, "Orders");
    assert!(again[0].password.is_none());
}

#[test]
fn an_ssh_key_tunnel_becomes_a_network_profile_without_a_password() {
    let mut connection = imported("Behind bastion");
    connection.host = "postgres".into();
    connection.ssh = Some(ImportedSsh {
        host: "127.0.0.1".into(),
        port: Some(2221),
        user: "spelaion".into(),
        auth: spelaion_config::SshAuth::Key,
        key_path: "C:\\keys\\id_ed25519".into(),
        password: None,
        keepalive_interval: None,
        keepalive_count: None,
        connect_timeout: None,
        local_port: None,
    });
    let name = connection.tunnel_name().unwrap();
    assert_eq!(name, "spelaion-127-0-0-1");
    let plan = import_plan(
        &connection,
        Engine::Postgres,
        Source::User,
        true,
        &name,
        &top_level(),
    );
    let (tunnel, secrets) = plan.tunnel.unwrap();
    assert_eq!(tunnel.hops[0].identity_file, "C:\\keys\\id_ed25519");
    assert_eq!(tunnel.hops[0].password, PasswordChoice::None);
    assert!(secrets.hops[0].is_none());
    assert_eq!(plan.connection.tunnel, name);
    let _: &ConnectionDraft = &plan.connection;
}
