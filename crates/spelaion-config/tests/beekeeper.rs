//! Beekeeper Studio's import (`beekeeper.rs`) against what a real Beekeeper Studio 6.2.2 for
//! Windows wrote (`tests/fixtures/beekeeper`, see its README): the saved connections and
//! folders of its `app.db`. A folder, PostgreSQL with TLS certificate files, PostgreSQL through
//! an SSH tunnel with a key file, a read-only MySQL with TLS and a colour, MariaDB, SQL Server,
//! two SQLite files and an Amazon Redshift connection SpelaionDB cannot open. Passwords are
//! decrypted the way Beekeeper decrypts them, with the install's `.key`.

mod common;

use std::path::{Path, PathBuf};

use common::{FakeBackend, Fixture};
use spelaion_config::{
    BeekeeperError, Engine, ImportedConnection, Source, SshAuth, TargetFolder, TlsModeConfig,
    add_connection, add_tunnel, import_plan, read_beekeeper,
};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/beekeeper")
}

/// The ciphertexts in `passwords.json`, made with Beekeeper's own encryptor and the
/// fixture's `.key` (see the README).
fn ciphertext(column: &str) -> String {
    let text = std::fs::read_to_string(fixtures().join("passwords.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json[column].as_str().unwrap().to_string()
}

/// Beekeeper's data folder: an `app.db` holding the fixture's tables plus `extra` SQL, and
/// the fixture's `.key` when `key` is set.
fn data_dir(fx: &Fixture, extra: &str, key: bool) -> PathBuf {
    let dir = fx.base.join("beekeeper-studio");
    std::fs::create_dir_all(&dir).unwrap();
    let db = rusqlite::Connection::open(dir.join("app.db")).unwrap();
    let sql = std::fs::read_to_string(fixtures().join("beekeeper-6.2.2.sql")).unwrap();
    db.execute_batch(&sql).unwrap();
    db.execute_batch(extra).unwrap();
    if key {
        std::fs::copy(fixtures().join(".key"), dir.join(".key")).unwrap();
    }
    dir
}

fn read_fixture(fx: &Fixture) -> Vec<ImportedConnection> {
    read_beekeeper(&data_dir(fx, "", true)).unwrap()
}

fn named<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
    all.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not found"))
}

const KEYS: &str = "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\";

const PASSPHRASE: &str = "SSH: if the private key has a passphrase, it is asked when connecting";

#[test]
fn every_connection_comes_with_its_engine_address_user_and_database() {
    let fx = Fixture::new("beekeeper-read");
    let all = read_fixture(&fx);
    let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Demo Database",
            "Orders TLS",
            "PG via bastion",
            "Shop MySQL",
            "Shop MariaDB",
            "Reports MSSQL",
            "Shop file",
            "Warehouse"
        ]
    );
    let expect = |name: &str, engine: Engine, host: &str, port: u16, database: &str, user: &str| {
        let c = named(&all, name);
        assert_eq!(c.engine, Ok(engine), "{name}");
        assert_eq!(c.host, host, "{name}");
        assert_eq!(c.port, Some(port), "{name}");
        assert_eq!(c.database, database, "{name}");
        assert_eq!(c.user, user, "{name}");
        assert!(!c.has_password(), "{name}");
        assert!(c.env.is_empty() && c.proxy.is_none(), "{name}");
        assert!(c.description.is_empty() && c.tags.is_empty(), "{name}");
    };
    expect(
        "Orders TLS",
        Engine::Postgres,
        "127.0.0.1",
        15432,
        "spelaion",
        "spelaion",
    );
    // Through the tunnel, the database's address as the bastion sees it.
    expect(
        "PG via bastion",
        Engine::Postgres,
        "postgres",
        5432,
        "spelaion",
        "spelaion",
    );
    expect(
        "Shop MySQL",
        Engine::Mysql,
        "127.0.0.1",
        13306,
        "spelaion",
        "spelaion",
    );
    expect(
        "Shop MariaDB",
        Engine::Mariadb,
        "127.0.0.1",
        13307,
        "spelaion",
        "root",
    );
    expect(
        "Reports MSSQL",
        Engine::Sqlserver,
        "127.0.0.1",
        11433,
        "master",
        "sa",
    );
    assert_eq!(named(&all, "Demo Database").id, "1");
    assert_eq!(named(&all, "Warehouse").id, "8");
}

#[test]
fn sqlite_files_come_with_their_path_only() {
    let fx = Fixture::new("beekeeper-sqlite");
    let all = read_fixture(&fx);
    let shop = named(&all, "Shop file");
    assert_eq!(shop.engine, Ok(Engine::Sqlite));
    assert_eq!(
        shop.database,
        "C:\\Users\\tester\\spelaiondb\\examples\\demo\\data\\shop.db"
    );
    // Beekeeper writes host "localhost" for files too: it means nothing.
    assert!(shop.host.is_empty() && shop.port.is_none() && shop.user.is_empty());
    assert!(shop.tls.is_none() && shop.ssh.is_none());
    // The SQLite "Create" flag is Beekeeper's own: nothing to say.
    assert!(shop.unmapped.is_empty(), "{:?}", shop.unmapped);
    // Beekeeper's own demo database is a connection like any other.
    let demo = named(&all, "Demo Database");
    assert_eq!(demo.engine, Ok(Engine::Sqlite));
    assert!(demo.database.ends_with("beekeeper-studio\\demo.db"));
}

#[test]
fn an_engine_spelaiondb_cannot_open_is_listed_but_not_imported() {
    let fx = Fixture::new("beekeeper-redshift");
    let all = read_fixture(&fx);
    let warehouse = named(&all, "Warehouse");
    assert_eq!(
        warehouse.engine,
        Err("Amazon Redshift is not supported yet".to_string())
    );
    assert!(warehouse.host.is_empty() && warehouse.port.is_none());
    assert!(warehouse.unmapped.is_empty(), "{:?}", warehouse.unmapped);
}

#[test]
fn folders_come_along() {
    let fx = Fixture::new("beekeeper-folders");
    let all = read_fixture(&fx);
    assert_eq!(named(&all, "Orders TLS").folder, "Production");
    assert_eq!(named(&all, "PG via bastion").folder, "Production");
    for name in ["Demo Database", "Shop MySQL", "Reports MSSQL", "Warehouse"] {
        assert!(named(&all, name).folder.is_empty(), "{name}");
    }
}

#[test]
fn tls_follows_what_beekeeper_applies() {
    let fx = Fixture::new("beekeeper-tls");
    let all = read_fixture(&fx);
    // Certificate files with "Reject Unauthorized": Node checks the chain and the host name.
    let orders = named(&all, "Orders TLS").tls.clone().unwrap();
    assert_eq!(orders.mode, TlsModeConfig::VerifyFull);
    assert_eq!(orders.ca, format!("{KEYS}ca.crt"));
    assert_eq!(orders.cert, format!("{KEYS}client.crt"));
    assert_eq!(orders.key, format!("{KEYS}client.key"));
    // TLS without files: Beekeeper never verifies the server's certificate then.
    let mysql = named(&all, "Shop MySQL").tls.clone().unwrap();
    assert_eq!(mysql.mode, TlsModeConfig::Require);
    assert!(mysql.ca.is_empty() && mysql.cert.is_empty() && mysql.key.is_empty());
    // TLS off, and SQL Server trusting its certificate: SpelaionDB's default.
    for name in ["PG via bastion", "Shop MariaDB", "Reports MSSQL"] {
        assert!(named(&all, name).tls.is_none(), "{name}");
    }
}

#[test]
fn the_ssh_tunnel_becomes_a_one_hop_tunnel_with_its_key_file() {
    let fx = Fixture::new("beekeeper-ssh");
    let all = read_fixture(&fx);
    let bastion = named(&all, "PG via bastion");
    let ssh = bastion.ssh.clone().unwrap();
    assert_eq!(ssh.host, "127.0.0.1");
    assert_eq!(ssh.port, Some(2221));
    assert_eq!(ssh.user, "spelaion");
    assert_eq!(ssh.auth, SshAuth::Key);
    assert_eq!(ssh.key_path, format!("{KEYS}id_ed25519"));
    assert_eq!(ssh.keepalive_interval, Some(30));
    assert!(ssh.password.is_none());
    assert_eq!(bastion.tunnel_name().as_deref(), Some("spelaion-127-0-0-1"));
    assert_eq!(bastion.unmapped, [PASSPHRASE]);
    // Beekeeper keeps "agent" and a key path default on every connection: no tunnel unless on.
    assert_eq!(all.iter().filter(|c| c.ssh.is_some()).count(), 1);
}

#[test]
fn read_only_comes_along_and_the_colour_is_listed_not_imported() {
    let fx = Fixture::new("beekeeper-colour");
    let all = read_fixture(&fx);
    let mysql = named(&all, "Shop MySQL");
    assert!(mysql.read_only);
    assert_eq!(mysql.unmapped, ["Colour: red"]);
    let orders = named(&all, "Orders TLS");
    assert!(!orders.read_only);
    assert_eq!(orders.unmapped, ["Colour: green"]);
    for name in ["Shop MariaDB", "Reports MSSQL"] {
        assert!(named(&all, name).unmapped.is_empty(), "{name}");
    }
}

// ----------------------------------------------------------------------------- passwords

fn with_saved_passwords(fx: &Fixture, key: bool) -> Vec<ImportedConnection> {
    let extra = format!(
        "UPDATE saved_connection SET password = '{}' WHERE name = 'Orders TLS';
         UPDATE saved_connection SET sshKeyfilePassword = '{}' WHERE name = 'PG via bastion';
         UPDATE saved_connection SET sshMode = 'userpass', sshKeyfile = NULL, sshPassword = '{}',
             password = '{}' WHERE name = 'Shop MariaDB';
         UPDATE saved_connection SET sshEnabled = 1, sshHost = 'jump.example', sshPort = 22,
             sshUsername = 'ops' WHERE name = 'Shop MariaDB';",
        ciphertext("password"),
        ciphertext("sshKeyfilePassword"),
        ciphertext("sshPassword"),
        ciphertext("password"),
    );
    read_beekeeper(&data_dir(fx, &extra, key)).unwrap()
}

#[test]
fn saved_passwords_are_decrypted_with_the_installs_key() {
    let fx = Fixture::new("beekeeper-passwords");
    let all = with_saved_passwords(&fx, true);
    let orders = named(&all, "Orders TLS");
    assert_eq!(
        orders.password.as_ref().map(|s| s.expose().to_string()),
        Some("fake-password-123".to_string())
    );
    assert!(orders.stored_password.is_none());
    assert_eq!(orders.unmapped, ["Colour: green"]);
    // An SSH password tunnel: its password too.
    let mariadb = named(&all, "Shop MariaDB");
    let ssh = mariadb.ssh.clone().unwrap();
    assert_eq!(ssh.auth, SshAuth::Password);
    assert!(ssh.key_path.is_empty());
    assert_eq!(
        ssh.password.as_ref().map(|s| s.expose().to_string()),
        Some("fake-ssh-password".to_string())
    );
    assert!(mariadb.password.is_some());
    // A key's passphrase is never decrypted: SpelaionDB asks for it when connecting.
    let bastion = named(&all, "PG via bastion");
    assert!(bastion.ssh.as_ref().unwrap().password.is_none());
    assert_eq!(
        bastion.unmapped,
        [
            "SSH: Beekeeper saved the key's passphrase, which is not imported; it is asked when connecting"
        ]
    );
    // No saved secret reaches anything the preview shows.
    let everything = format!("{all:?}");
    for secret in ["fake-password-123", "fake-ssh-password", "fake-passphrase"] {
        assert!(!everything.contains(secret), "{secret}");
    }
}

#[test]
fn without_the_key_file_passwords_are_listed_and_asked() {
    let fx = Fixture::new("beekeeper-no-key");
    let all = with_saved_passwords(&fx, false);
    let orders = named(&all, "Orders TLS");
    assert!(orders.password.is_none());
    assert!(
        orders
            .unmapped
            .iter()
            .any(|u| u.starts_with("Password: Beekeeper saved one")
                && u.contains(".key file, which was not found")),
        "{:?}",
        orders.unmapped
    );
    let mariadb = named(&all, "Shop MariaDB");
    assert!(mariadb.ssh.as_ref().unwrap().password.is_none());
    assert!(
        mariadb
            .unmapped
            .iter()
            .any(|u| u.starts_with("SSH: Beekeeper saved the SSH password")),
        "{:?}",
        mariadb.unmapped
    );
}

#[test]
fn a_damaged_key_or_another_installs_key_decrypts_nothing() {
    let fx = Fixture::new("beekeeper-bad-key");
    let extra = format!(
        "UPDATE saved_connection SET password = '{}' WHERE name = 'Orders TLS';",
        ciphertext("password")
    );
    let dir = data_dir(&fx, &extra, true);
    // One changed character: the HMAC refuses it before anything is decrypted.
    let key = std::fs::read_to_string(dir.join(".key")).unwrap();
    let flipped = format!(
        "{}{}",
        if key.starts_with('0') { '1' } else { '0' },
        &key[1..]
    );
    std::fs::write(dir.join(".key"), flipped).unwrap();
    let all = read_beekeeper(&dir).unwrap();
    let orders = named(&all, "Orders TLS");
    assert!(orders.password.is_none());
    assert!(
        orders
            .unmapped
            .iter()
            .any(|u| u.contains("which did not open")),
        "{:?}",
        orders.unmapped
    );
    // A key that opens, but is another install's: the password does not decrypt with it.
    let other_key = format!(
        "{}{}",
        "0".repeat(64),
        "not an encrypted key at all, just text that is long enough"
    );
    std::fs::write(dir.join(".key"), other_key).unwrap();
    assert!(
        named(&read_beekeeper(&dir).unwrap(), "Orders TLS")
            .unmapped
            .iter()
            .any(|u| u.contains("which did not open"))
    );
    let fx2 = Fixture::new("beekeeper-other-install");
    let dir2 = data_dir(
        &fx2,
        // A value encrypted with another install's key: its HMAC does not match this one's.
        "UPDATE saved_connection SET password = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855000102030405060708090a0b0c0d0e0fAAAAAAAAAAAAAAAAAAAAAA==' WHERE name = 'Orders TLS';",
        true,
    );
    let orders = named(&read_beekeeper(&dir2).unwrap(), "Orders TLS").clone();
    assert!(orders.password.is_none());
    assert!(
        orders
            .unmapped
            .iter()
            .any(|u| u.contains("does not decrypt with this install's key")),
        "{:?}",
        orders.unmapped
    );
}

#[test]
fn every_connection_becomes_a_connection_file_and_the_password_goes_to_the_keychain() {
    let fx = Fixture::new("beekeeper-import");
    let backend = FakeBackend::default();
    let folder = TargetFolder {
        folder: "Beekeeper".into(),
        keep_client_folders: true,
    };
    let mut imported = 0;
    for connection in with_saved_passwords(&fx, true) {
        let Ok(engine) = connection.engine.clone() else {
            continue;
        };
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
        imported += 1;
    }
    assert_eq!(imported, 7);
    assert_eq!(fx.config().connections.len(), 7);
    let orders = fx.read("user/connections/beekeeper/production/orders-tls.toml");
    assert!(
        orders.contains("verify-full")
            && orders.contains("ca.crt")
            && orders.contains("client.key"),
        "{orders}"
    );
    let bastion = fx.read("user/connections/beekeeper/production/pg-via-bastion.toml");
    assert!(
        bastion.contains("tunnel = \"spelaion-127-0-0-1\""),
        "{bastion}"
    );
    let tunnel = fx.read("user/tunnels/spelaion-127-0-0-1.toml");
    assert!(tunnel.contains("id_ed25519"), "{tunnel}");
    let mysql = fx.read("user/connections/beekeeper/shop-mysql.toml");
    assert!(mysql.contains("read_only = true"), "{mysql}");
    // No colour reached a file (ADR-029).
    assert!(!mysql.contains("\"red\""), "{mysql}");
    // The passwords went to the keychain, never to a file.
    let keychain = backend.keychain.lock().unwrap();
    let values: Vec<&String> = keychain.values().collect();
    assert!(
        values.iter().any(|v| *v == "fake-password-123"),
        "{values:?}"
    );
    assert!(
        values.iter().any(|v| *v == "fake-ssh-password"),
        "{values:?}"
    );
    for (path, text) in [("orders", &orders), ("mysql", &mysql), ("tunnel", &tunnel)] {
        assert!(!text.contains("fake-"), "{path}: {text}");
    }
}

// ------------------------------------------------------------- rows no fixture covers

#[test]
fn settings_beekeeper_has_and_spelaiondb_does_not_are_listed() {
    let fx = Fixture::new("beekeeper-crafted");
    let dir = data_dir(
        &fx,
        "INSERT INTO connection_folder (id, name, parentId) VALUES (2, 'EU', 1), (3, 'a/b', 2);
         INSERT INTO saved_connection (version, connectionType, host, port, username, defaultDatabase,
             uniqueHash, name, sshEnabled, sshHost, sshPort, sshMode, sshUsername,
             sshBastionHost, sshBastionHostPort, sshBastionUsername, labelColor, domain, url,
             options, sqlServerOptions, ssl, sslCaFile, sslCertFile, sslKeyFile,
             sslRejectUnauthorized, trustServerCertificate, workspaceId, connectionFolderId,
             path)
           VALUES (1, 'sqlserver', 'mssql.internal', 1433, 'app', 'sales', 'DEPRECATED',
             'Crafted', 1, 'ssh.internal', 22, 'agent', 'ops', 'jump.example', 2222, 'gate',
             'purple', 'CORP', 'not-really-encrypted',
             '{\"connectionMethod\": \"manual\", \"privilege\": \"SYSDBA\", \"apiToken\": \"tok-secret\"}',
             '{\"encryptionMode\": \"strict\"}', 1, '/c/ca.pem', '/c/c.pem', '/c/k.pem', 0, 0, 7,
             3, '/old/path');
         INSERT INTO saved_connection (version, connectionType, defaultDatabase, uniqueHash, name)
           VALUES (1, 'duckdb', 'C:\\data\\sales.duckdb', 'DEPRECATED', 'Analytics');
         INSERT INTO saved_connection (version, connectionType, host, uniqueHash, name,
             socketPathEnabled, socketPath)
           VALUES (1, 'postgresql', 'localhost', 'DEPRECATED', 'Socket', 1, '/var/run/postgresql');
         INSERT INTO saved_connection (version, connectionType, host, uniqueHash, name,
             windowsAuthEnabled)
           VALUES (1, 'sqlserver', 'win.internal', 'DEPRECATED', 'Windows auth', 1);",
        true,
    );
    let all = read_beekeeper(&dir).unwrap();
    let c = named(&all, "Crafted");
    // Nested folders, and a slash inside a name is not a level.
    assert_eq!(c.folder, "Production/EU/a-b");
    let ssh = c.ssh.clone().unwrap();
    assert_eq!(ssh.auth, SshAuth::Agent);
    assert_eq!(
        (ssh.host.as_str(), ssh.port, ssh.user.as_str()),
        ("ssh.internal", Some(22), "ops")
    );
    // All three files without "Reject Unauthorized": the certificate is trusted.
    let tls = c.tls.clone().unwrap();
    assert_eq!(tls.mode, TlsModeConfig::Require);
    assert_eq!(tls.ca, "/c/ca.pem");
    for expected in [
        "Beekeeper workspace: 7",
        "SSH: jump host gate@jump.example:2222 before the SSH host (add it as the tunnel's first hop after importing)",
        "Colour: purple",
        "Domain: CORP",
        "URL: Beekeeper saved one, encrypted; it is not read",
        "Option: privilege=SYSDBA",
        "Option: apiToken (value not shown)",
        "SQL Server option: encryptionMode=strict",
        "Option: path=/old/path",
    ] {
        assert!(
            c.unmapped.iter().any(|u| u == expected),
            "{expected}: {:?}",
            c.unmapped
        );
    }
    for secret in ["tok-secret", "not-really-encrypted"] {
        assert!(
            !format!("{:?}", c.unmapped).contains(secret),
            "{secret}: {:?}",
            c.unmapped
        );
    }
    let duck = named(&all, "Analytics");
    assert_eq!(duck.engine, Ok(Engine::Duckdb));
    assert_eq!(duck.database, "C:\\data\\sales.duckdb");
    let socket = named(&all, "Socket").engine.clone().unwrap_err();
    assert!(socket.contains("local socket"), "{socket}");
    let windows = named(&all, "Windows auth").engine.clone().unwrap_err();
    assert!(windows.contains("Windows authentication"), "{windows}");
}

#[test]
fn sql_server_that_does_not_trust_its_certificate_verifies_it() {
    let fx = Fixture::new("beekeeper-mssql-verify");
    let dir = data_dir(
        &fx,
        "UPDATE saved_connection SET trustServerCertificate = 0 WHERE name = 'Reports MSSQL';",
        true,
    );
    let all = read_beekeeper(&dir).unwrap();
    let tls = named(&all, "Reports MSSQL").tls.clone().unwrap();
    assert_eq!(tls.mode, TlsModeConfig::VerifyFull);
}

#[test]
fn databases_that_are_not_beekeepers_are_refused() {
    let fx = Fixture::new("beekeeper-bad");
    let other = fx.base.join("other").join("app.db");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    rusqlite::Connection::open(&other)
        .unwrap()
        .execute_batch("CREATE TABLE t (x)")
        .unwrap();
    assert!(matches!(
        read_beekeeper(&other),
        Err(BeekeeperError::Unreadable { .. })
    ));
    let text = fx.write("text/app.db", "not a database at all");
    assert!(matches!(
        read_beekeeper(&text),
        Err(BeekeeperError::Unreadable { .. })
    ));
    let empty = fx.base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(matches!(
        read_beekeeper(&empty),
        Err(BeekeeperError::NotFound(_))
    ));
    assert!(matches!(
        read_beekeeper(Path::new("C:/does/not/exist/app.db")),
        Err(BeekeeperError::NotFound(_))
    ));
}

#[test]
fn the_database_file_itself_reads_like_the_folder_and_is_never_changed() {
    let fx = Fixture::new("beekeeper-readonly");
    let dir = data_dir(&fx, "", true);
    let file = dir.join("app.db");
    let before = std::fs::read(&file).unwrap();
    let from_file = read_beekeeper(&file).unwrap();
    assert_eq!(from_file.len(), read_beekeeper(&dir).unwrap().len());
    assert_eq!(std::fs::read(&file).unwrap(), before);
    // Nothing next to it either (no journal or WAL left behind).
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".key", "app.db"]);
}
