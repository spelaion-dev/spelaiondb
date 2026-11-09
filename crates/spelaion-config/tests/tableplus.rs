//! TablePlus's import (`tableplus.rs`) against files a real TablePlus 27.2 for Windows wrote
//! (`tests/fixtures/tableplus`, see its README): its own storage and its encrypted export of the
//! same connections. Every supported engine, an SSH tunnel with a private key, TLS with
//! certificate files, environment tags (production among them), a group and a read-only DuckDB
//! file. Passwords in TablePlus's own store are never read; the export's come with its password
//! and only go to the keychain.

mod common;

use std::path::PathBuf;

use common::{FakeBackend, Fixture};
use spelaion_config::{
    Engine, ImportedConnection, Source, SshAuth, TableplusError, TargetFolder, TlsModeConfig,
    add_connection, add_tunnel, import_plan, read_tableplus,
};
use spelaion_driver_api::Secret;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tableplus")
}

/// TablePlus's folder, as `%LOCALAPPDATA%\com.tinyapp.TablePlus`.
fn app_dir() -> PathBuf {
    fixtures().join("com.tinyapp.TablePlus")
}

fn read() -> Vec<ImportedConnection> {
    read_tableplus(&app_dir(), None).unwrap()
}

fn named<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
    all.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not found"))
}

const PASSWORD: &str = "Password: if TablePlus saved one, it is in its own encrypted store, which SpelaionDB does not read; it is asked when connecting";

#[test]
fn every_engine_comes_with_its_address_and_user() {
    let all = read();
    let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Orders PG",
            "Shop MySQL",
            "Shop MariaDB",
            "Reports MSSQL",
            "Local file",
            "Analytics",
            "PG via bastion"
        ]
    );
    let expect =
        |name: &str, engine: Engine, host: &str, port: Option<u16>, db: &str, user: &str| {
            let c = named(&all, name);
            assert_eq!(c.engine, Ok(engine), "{name}");
            assert_eq!(c.host, host, "{name}");
            assert_eq!(c.port, port, "{name}");
            assert_eq!(c.database, db, "{name}");
            assert_eq!(c.user, user, "{name}");
            assert!(
                c.password.is_none() && c.stored_password.is_none(),
                "{name}"
            );
            assert!(c.description.is_empty() && c.tags.is_empty(), "{name}");
        };
    expect(
        "Orders PG",
        Engine::Postgres,
        "127.0.0.1",
        Some(15432),
        "spelaion",
        "spelaion",
    );
    expect(
        "Shop MySQL",
        Engine::Mysql,
        "127.0.0.1",
        Some(13306),
        "spelaion",
        "spelaion",
    );
    expect(
        "Shop MariaDB",
        Engine::Mariadb,
        "127.0.0.1",
        Some(13307),
        "spelaion",
        "root",
    );
    expect(
        "Reports MSSQL",
        Engine::Sqlserver,
        "127.0.0.1",
        Some(11433),
        "master",
        "sa",
    );
    // TablePlus writes the file in `DatabaseHost` and a meaningless port 5432 for files.
    expect(
        "Local file",
        Engine::Sqlite,
        "",
        None,
        "C:\\data\\local.db",
        "",
    );
    expect(
        "Analytics",
        Engine::Duckdb,
        "",
        None,
        "C:\\data\\sales.duckdb",
        "",
    );
    expect(
        "PG via bastion",
        Engine::Postgres,
        "postgres",
        Some(5432),
        "spelaion",
        "spelaion",
    );

    // Its `Data` folder and the file itself give the same connections, with TablePlus's ids.
    for path in [
        app_dir().join("Data"),
        app_dir().join("Data/Connections.plist"),
    ] {
        let again = read_tableplus(&path, None).unwrap();
        assert_eq!(again.len(), all.len(), "{}", path.display());
    }
    assert_eq!(
        named(&all, "Orders PG").id,
        "CD5FB5DF-E0B5-406B-8688-62EB3F62E0B1"
    );
    // A folder without TablePlus's files says where they were looked for.
    let error = read_tableplus(&fixtures(), None).unwrap_err().to_string();
    assert!(error.contains("Connections.plist"), "{error}");
}

#[test]
fn environment_tags_become_connection_types() {
    let all = read();
    let env = |name: &str| named(&all, name).env.clone();
    assert_eq!(env("Orders PG"), "prod");
    assert_eq!(env("Shop MySQL"), "dev");
    assert_eq!(env("Shop MariaDB"), "test");
    assert_eq!(env("Reports MSSQL"), "test");
    // TablePlus's `local` tag is development.
    assert_eq!(env("Local file"), "dev");
}

#[test]
fn groups_become_folders() {
    let all = read();
    assert_eq!(named(&all, "Orders PG").folder, "Prod");
    assert!(
        all.iter()
            .filter(|c| c.name != "Orders PG")
            .all(|c| c.folder.is_empty())
    );
}

#[test]
fn tls_comes_with_its_mode_and_certificate_files() {
    let all = read();
    let tls = named(&all, "Orders PG").tls.clone().unwrap();
    assert_eq!(tls.mode, TlsModeConfig::VerifyCa);
    let keys = "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\";
    assert_eq!(tls.ca, format!("{keys}ca.crt"));
    assert_eq!(tls.cert, format!("{keys}client.crt"));
    assert_eq!(tls.key, format!("{keys}client.key"));
    // MySQL's REQUIRED, MariaDB's VERIFY-SERVER-CERT; no files.
    let mysql = named(&all, "Shop MySQL").tls.clone().unwrap();
    assert_eq!(mysql.mode, TlsModeConfig::Require);
    assert!(mysql.ca.is_empty() && mysql.cert.is_empty() && mysql.key.is_empty());
    assert_eq!(
        named(&all, "Shop MariaDB").tls.clone().unwrap().mode,
        TlsModeConfig::VerifyFull
    );
    // TablePlus's PREFERRED is SpelaionDB's default; files and SQL Server have no TLS settings.
    for name in ["PG via bastion", "Reports MSSQL", "Local file", "Analytics"] {
        assert!(named(&all, name).tls.is_none(), "{name}");
    }
}

#[test]
fn the_ssh_tunnel_uses_key_authentication_without_the_key_tableplus_keeps() {
    let all = read();
    let bastion = named(&all, "PG via bastion");
    let ssh = bastion.ssh.clone().unwrap();
    assert_eq!(ssh.host, "127.0.0.1");
    assert_eq!(ssh.port, Some(2221));
    assert_eq!(ssh.user, "spelaion");
    assert_eq!(ssh.auth, SshAuth::Key);
    // TablePlus keeps only the key's name: SpelaionDB's usual keys until the user picks it.
    assert!(ssh.key_path.is_empty());
    assert!(ssh.password.is_none());
    assert!(
        bastion
            .unmapped
            .iter()
            .any(|u| u.starts_with("SSH: ") && u.contains("id_ed25519")),
        "{:?}",
        bastion.unmapped
    );
    assert_eq!(all.iter().filter(|c| c.ssh.is_some()).count(), 1);
}

#[test]
fn what_is_not_imported_is_listed_and_passwords_are_not_read() {
    let all = read();
    let orders = named(&all, "Orders PG");
    assert_eq!(
        orders.unmapped,
        [
            PASSWORD,
            "Bootstrap commands: 1 line(s) TablePlus runs after connecting (add them to the connection's session setup)"
        ]
    );
    // The bootstrap SQL itself is not shown.
    assert!(!orders.unmapped.iter().any(|u| u.contains("search_path")));
    for name in ["Shop MySQL", "Shop MariaDB", "Reports MSSQL"] {
        assert_eq!(named(&all, name).unmapped, [PASSWORD], "{name}");
    }
    // Files have no password.
    for name in ["Local file", "Analytics"] {
        assert!(named(&all, name).unmapped.is_empty(), "{name}");
    }
    assert!(named(&all, "Analytics").read_only);
    assert!(all.iter().filter(|c| c.read_only).count() == 1);
}

#[test]
fn every_connection_becomes_a_connection_file_and_the_bastion_a_network_profile() {
    let fx = Fixture::new("tableplus-import");
    let backend = FakeBackend::default();
    let folder = TargetFolder {
        folder: "TablePlus".into(),
        keep_client_folders: true,
    };
    for connection in read() {
        let engine = connection.engine.clone().unwrap();
        let tunnel_name = connection.tunnel_name().unwrap_or_default();
        let plan = import_plan(
            &connection,
            engine,
            Source::User,
            true,
            &tunnel_name,
            &folder,
        );
        if let Some((tunnel, passwords)) = &plan.tunnel {
            add_tunnel(&fx.config(), tunnel, passwords, &backend).unwrap();
        }
        add_connection(
            &fx.config(),
            &plan.connection,
            plan.password.as_ref(),
            &backend,
        )
        .unwrap_or_else(|e| panic!("{}: {e}", connection.name));
    }
    let config = fx.config();
    assert_eq!(config.connections.len(), 7);
    let orders = fx.read("user/connections/tableplus/prod/orders-pg.toml");
    assert!(orders.contains("env = \"prod\""), "{orders}");
    assert!(
        orders.contains("ca.crt") && orders.contains("client.key"),
        "{orders}"
    );
    let bastion = fx.read("user/connections/tableplus/pg-via-bastion.toml");
    assert!(
        bastion.contains("tunnel = \"spelaion-127-0-0-1\""),
        "{bastion}"
    );
    let tunnel = fx.read("user/tunnels/spelaion-127-0-0-1.toml");
    assert!(tunnel.contains("key"), "{tunnel}");
    assert!(!tunnel.contains("identity_file"), "{tunnel}");
    let analytics = fx.read("user/connections/tableplus/analytics.toml");
    assert!(analytics.contains("read_only = true"), "{analytics}");
    // No password reached a file or the keychain: TablePlus's stay in its own store.
    assert!(backend.keychain.lock().unwrap().is_empty());
}

// ------------------------------------------------------------------------------- export

/// The password the fixture was exported with (made up for it, see the README).
const EXPORT_PASSWORD: &str = "fixture-export-pw";

fn export_file() -> PathBuf {
    fixtures().join("export.tableplusconnection")
}

fn read_export() -> Vec<ImportedConnection> {
    read_tableplus(&export_file(), Some(&Secret::new(EXPORT_PASSWORD))).unwrap()
}

#[test]
fn the_export_needs_its_password_and_refuses_a_wrong_one() {
    for password in [None, Some(Secret::new(""))] {
        let error = read_tableplus(&export_file(), password.as_ref()).unwrap_err();
        assert!(matches!(error, TableplusError::NeedsPassword(_)), "{error}");
    }
    for wrong in ["fixture-export-PW", "fixture-export-pw ", "x"] {
        let error = read_tableplus(&export_file(), Some(&Secret::new(wrong))).unwrap_err();
        assert!(
            matches!(error, TableplusError::WrongPassword(_)),
            "{wrong}: {error}"
        );
        // The message names the file, never the password.
        assert!(!error.to_string().contains("fixture-export"), "{error}");
    }
}

#[test]
fn a_changed_or_cut_export_is_refused_before_anything_is_read() {
    let fx = Fixture::new("tableplus-tampered");
    let original = std::fs::read(export_file()).unwrap();
    let password = Secret::new(EXPORT_PASSWORD);
    // One flipped bit anywhere, header, ciphertext or tag: the HMAC no longer matches.
    for at in [5, 40, original.len() / 2, original.len() - 1] {
        let mut changed = original.clone();
        changed[at] ^= 0x01;
        let path = fx.base.join(format!("changed-{at}.tableplusconnection"));
        std::fs::write(&path, &changed).unwrap();
        let error = read_tableplus(&path, Some(&password)).unwrap_err();
        assert!(
            matches!(error, TableplusError::WrongPassword(_)),
            "byte {at}: {error}"
        );
    }
    // Cut short, or not a TablePlus file at all.
    for (name, bytes) in [
        ("cut", original[..60].to_vec()),
        ("empty", Vec::new()),
        ("other", b"PK\x03\x04 a zip".to_vec()),
    ] {
        let path = fx.base.join(format!("{name}.tableplusconnection"));
        std::fs::write(&path, &bytes).unwrap();
        let error = read_tableplus(&path, Some(&password)).unwrap_err();
        assert!(
            matches!(error, TableplusError::Unreadable { .. }),
            "{name}: {error}"
        );
    }
    // A file far larger than any export is not read.
    let path = fx.base.join("huge.tableplusconnection");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(17 * 1024 * 1024).unwrap();
    drop(file);
    assert!(matches!(
        read_tableplus(&path, Some(&password)).unwrap_err(),
        TableplusError::Unreadable { .. }
    ));
}

#[test]
fn the_export_gives_the_same_connections_with_the_passwords_it_holds() {
    let storage = read();
    let export = read_export();
    let names = |all: &[ImportedConnection]| -> Vec<String> {
        all.iter().map(|c| c.name.clone()).collect()
    };
    assert_eq!(names(&export), names(&storage));
    for (a, b) in export.iter().zip(&storage) {
        assert_eq!(a.engine, b.engine, "{}", a.name);
        assert_eq!(
            (
                &a.host,
                a.port,
                &a.database,
                &a.user,
                &a.env,
                &a.folder,
                a.read_only
            ),
            (
                &b.host,
                b.port,
                &b.database,
                &b.user,
                &b.env,
                &b.folder,
                b.read_only
            ),
            "{}",
            a.name
        );
        assert_eq!(a.tls, b.tls, "{}", a.name);
        assert_eq!(a.ssh.is_some(), b.ssh.is_some(), "{}", a.name);
        // Nothing says a password was left in TablePlus's store: the export has them.
        assert!(
            !a.unmapped.iter().any(|u| u.starts_with("Password: ")),
            "{}: {:?}",
            a.name,
            a.unmapped
        );
    }
    // The export has no ids: they are numbered in its order, the same on every read.
    let ids: Vec<String> = export.iter().map(|c| c.id.clone()).collect();
    assert_eq!(ids[0], "export-1");
    assert_eq!(
        read_export()
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>(),
        ids
    );

    let orders = named(&export, "Orders PG");
    assert_eq!(orders.folder, "Prod");
    assert_eq!(orders.env, "prod");
    assert_eq!(
        orders.password.as_ref().map(|p| p.expose().to_string()),
        Some("fake-password-123".to_string())
    );
    // Only Orders PG had one.
    assert_eq!(export.iter().filter(|c| c.has_password()).count(), 1);
    // No preview line shows it.
    for c in &export {
        assert!(
            !c.unmapped.iter().any(|u| u.contains("fake-password-123")),
            "{}: {:?}",
            c.name,
            c.unmapped
        );
    }

    // The export carries the SSH key's contents, which are never kept.
    let bastion = named(&export, "PG via bastion");
    let ssh = bastion.ssh.clone().unwrap();
    assert_eq!(ssh.auth, SshAuth::Key);
    assert!(ssh.key_path.is_empty() && ssh.password.is_none());
    assert!(
        bastion
            .unmapped
            .iter()
            .any(|u| u.contains("carries the private key itself")),
        "{:?}",
        bastion.unmapped
    );
    assert!(
        !bastion.unmapped.iter().any(|u| u.contains("PRIVATE KEY")),
        "{:?}",
        bastion.unmapped
    );

    // The same export, decrypted (`export.json`), reads the same without a password.
    let plain = read_tableplus(&fixtures().join("export.json"), None).unwrap();
    assert_eq!(names(&plain), names(&export));
    assert!(named(&plain, "Orders PG").has_password());
}

#[test]
fn exported_passwords_go_to_the_keychain_only_when_asked_and_never_to_a_file() {
    for save in [true, false] {
        let fx = Fixture::new(&format!("tableplus-export-{save}"));
        let backend = FakeBackend::default();
        let folder = TargetFolder::default();
        for connection in read_export() {
            let engine = connection.engine.clone().unwrap();
            let tunnel_name = connection.tunnel_name().unwrap_or_default();
            let plan = import_plan(
                &connection,
                engine,
                Source::User,
                save,
                &tunnel_name,
                &folder,
            );
            if let Some((tunnel, passwords)) = &plan.tunnel {
                add_tunnel(&fx.config(), tunnel, passwords, &backend).unwrap();
            }
            add_connection(
                &fx.config(),
                &plan.connection,
                plan.password.as_ref(),
                &backend,
            )
            .unwrap_or_else(|e| panic!("{}: {e}", connection.name));
        }
        let keychain = backend.keychain.lock().unwrap().clone();
        let stored: Vec<&String> = keychain.values().collect();
        if save {
            assert_eq!(stored, ["fake-password-123"], "{keychain:?}");
        } else {
            assert!(keychain.is_empty(), "{keychain:?}");
        }
        let orders = fx.read("user/connections/orders-pg.toml");
        assert!(!orders.contains("fake-password-123"), "{orders}");
        let tunnel = fx.read("user/tunnels/spelaion-127-0-0-1.toml");
        assert!(!tunnel.contains("PRIVATE KEY"), "{tunnel}");
    }
}
