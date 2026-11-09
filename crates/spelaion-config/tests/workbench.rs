//! MySQL Workbench's import (`workbench.rs`) against the `connections.xml` a real MySQL
//! Workbench 8.0.47 for Windows wrote (`tests/fixtures/workbench`, see its README): a direct
//! connection with a default schema, one with TLS (Require and Verify CA, certificate files)
//! and advanced settings in a group, one "Standard TCP/IP over SSH" with a key file, and a
//! named pipe one SpelaionDB cannot open. Passwords are read only from the macOS keychain, on
//! import; Workbench's Windows vault and Linux keychain items are never read.

mod common;

use std::path::PathBuf;

use common::{FakeBackend, Fixture};
use spelaion_config::{
    Engine, ImportedConnection, KeychainItem, Source, SshAuth, TargetFolder, TlsModeConfig,
    WorkbenchError, WorkbenchPasswords, add_connection, add_tunnel, import_plan,
    read_stored_passwords, read_workbench,
};

/// Workbench's folder, as `%APPDATA%\MySQL\Workbench`.
fn workbench_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workbench/Workbench")
}

fn read_as(passwords: WorkbenchPasswords) -> Vec<ImportedConnection> {
    read_workbench(&workbench_dir(), passwords).unwrap()
}

/// As Workbench for Windows wrote it (the fixture has no vault next to it).
fn read() -> Vec<ImportedConnection> {
    read_as(WorkbenchPasswords::Vault)
}

fn named<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
    all.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not found"))
}

const KEYS: &str = "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\";

const VAULT: &str = "Password: if Workbench saved one, it is in its vault (workbench_user_data.dat, protected with your Windows account), which SpelaionDB does not read; it is asked when connecting";

#[test]
fn every_connection_comes_with_its_address_user_and_default_schema() {
    let all = read();
    let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Shop MySQL",
            "Shop MariaDB TLS",
            "Orders via bastion",
            "Local pipe"
        ]
    );
    let expect = |name: &str, host: &str, port: u16, db: &str, user: &str| {
        let c = named(&all, name);
        // Workbench uses one driver for MySQL and MariaDB, and these never connected.
        assert_eq!(c.engine, Ok(Engine::Mysql), "{name}");
        assert_eq!(c.host, host, "{name}");
        assert_eq!(c.port, Some(port), "{name}");
        assert_eq!(c.database, db, "{name}");
        assert_eq!(c.user, user, "{name}");
        assert!(
            c.password.is_none() && c.stored_password.is_none(),
            "{name}"
        );
        assert!(c.description.is_empty() && c.tags.is_empty(), "{name}");
        assert!(
            c.env.is_empty() && !c.read_only && c.proxy.is_none(),
            "{name}"
        );
    };
    expect("Shop MySQL", "127.0.0.1", 13306, "spelaion", "spelaion");
    expect("Shop MariaDB TLS", "127.0.0.1", 13307, "spelaion", "root");
    // Over SSH, the database's address as the bastion sees it.
    expect("Orders via bastion", "mysql", 3306, "spelaion", "spelaion");

    assert_eq!(
        named(&all, "Shop MySQL").id,
        "{5315ECE4-5584-4E05-8C44-27C41DB40C54}"
    );
    // The file itself reads the same as Workbench's folder.
    let again = read_workbench(
        &workbench_dir().join("connections.xml"),
        WorkbenchPasswords::Vault,
    )
    .unwrap();
    assert_eq!(again.len(), all.len());
}

#[test]
fn a_named_pipe_connection_is_listed_but_cannot_be_imported() {
    let all = read();
    let pipe = named(&all, "Local pipe");
    let reason = pipe.engine.clone().unwrap_err();
    assert!(
        reason.contains("named pipe") && reason.contains("MySQL"),
        "{reason}"
    );
    assert!(pipe.host.is_empty() && pipe.port.is_none());
    assert!(pipe.tls.is_none() && pipe.ssh.is_none());
}

#[test]
fn groups_become_folders() {
    let all = read();
    assert_eq!(named(&all, "Shop MariaDB TLS").folder, "Prod");
    assert_eq!(named(&all, "Orders via bastion").folder, "Prod");
    assert!(named(&all, "Shop MySQL").folder.is_empty());
    assert!(named(&all, "Local pipe").folder.is_empty());
}

#[test]
fn tls_comes_with_its_mode_and_certificate_files() {
    let all = read();
    let tls = named(&all, "Shop MariaDB TLS").tls.clone().unwrap();
    assert_eq!(tls.mode, TlsModeConfig::VerifyCa);
    assert_eq!(tls.ca, format!("{KEYS}ca.crt"));
    assert_eq!(tls.cert, format!("{KEYS}client.crt"));
    assert_eq!(tls.key, format!("{KEYS}client.key"));
    // "If available" is SpelaionDB's default (prefer TLS).
    assert!(named(&all, "Shop MySQL").tls.is_none());
    assert!(named(&all, "Orders via bastion").tls.is_none());
}

#[test]
fn standard_tcp_ip_over_ssh_becomes_a_one_hop_tunnel_with_its_key_file() {
    let all = read();
    let bastion = named(&all, "Orders via bastion");
    let ssh = bastion.ssh.clone().unwrap();
    assert_eq!(ssh.host, "127.0.0.1");
    assert_eq!(ssh.port, Some(2221));
    assert_eq!(ssh.user, "spelaion");
    assert_eq!(ssh.auth, SshAuth::Key);
    assert_eq!(ssh.key_path, format!("{KEYS}id_ed25519"));
    assert!(ssh.password.is_none());
    assert_eq!(bastion.tunnel_name().as_deref(), Some("spelaion-127-0-0-1"));
    assert_eq!(all.iter().filter(|c| c.ssh.is_some()).count(), 1);
}

#[test]
fn what_is_not_imported_is_listed() {
    let all = read();
    assert_eq!(
        named(&all, "Shop MariaDB TLS").unmapped,
        [
            "SQL mode: ANSI_QUOTES (add SET SESSION sql_mode to the connection's session setup)",
            "Timeout: 20 s",
            "Option: OPT_LOCAL_INFILE=1",
        ]
    );
    assert!(named(&all, "Shop MySQL").unmapped.is_empty());
    assert_eq!(
        named(&all, "Orders via bastion").unmapped,
        ["SSH: if the private key has a passphrase, it is asked when connecting"]
    );
}

#[test]
fn on_windows_the_vault_is_never_read_and_the_preview_says_so() {
    let fx = Fixture::new("workbench-vault");
    let dir = fx.base.join("Workbench");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        workbench_dir().join("connections.xml"),
        dir.join("connections.xml"),
    )
    .unwrap();
    // Not a real vault: it is never opened, only noticed.
    std::fs::write(
        dir.join("workbench_user_data.dat"),
        b"\x01\x00\x00\x00not dpapi",
    )
    .unwrap();
    let all = read_workbench(&dir, WorkbenchPasswords::Vault).unwrap();
    for name in ["Shop MySQL", "Shop MariaDB TLS", "Orders via bastion"] {
        let c = named(&all, name);
        assert!(
            c.password.is_none() && c.stored_password.is_none(),
            "{name}"
        );
        assert!(
            c.unmapped.iter().any(|u| u == VAULT),
            "{name}: {:?}",
            c.unmapped
        );
    }
    // A connection that cannot be imported says nothing about its password.
    assert!(named(&all, "Local pipe").unmapped.is_empty());
}

#[test]
fn on_macos_passwords_come_from_the_keychain_only_when_importing_with_them() {
    let mut all = read_as(WorkbenchPasswords::Keychain);
    // Workbench's keychain items: its host identifier as the service, the user as the account.
    assert_eq!(
        named(&all, "Shop MySQL").stored_password,
        Some(KeychainItem {
            service: "Mysql@127.0.0.1:13306".into(),
            account: "spelaion".into(),
        })
    );
    assert_eq!(
        named(&all, "Orders via bastion").stored_password,
        Some(KeychainItem {
            service: "Mysql@mysql:3306@127.0.0.1:2221".into(),
            account: "spelaion".into(),
        })
    );
    assert!(named(&all, "Local pipe").stored_password.is_none());
    assert!(all.iter().all(|c| c.password.is_none()));
    assert!(named(&all, "Shop MySQL").has_password());
    // Nothing about the vault on a Mac.
    assert!(
        all.iter()
            .all(|c| !c.unmapped.iter().any(|u| u.starts_with("Password: ")))
    );

    let backend = FakeBackend::default();
    backend.other_apps.lock().unwrap().insert(
        ("Mysql@127.0.0.1:13306".into(), "spelaion".into()),
        "fake-password-123".into(),
    );
    let failed = read_stored_passwords(&mut all, &backend);
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(
        named(&all, "Shop MySQL")
            .password
            .as_ref()
            .map(|p| p.expose().to_string()),
        Some("fake-password-123".into())
    );
    // A missing item: asked when connecting.
    assert!(named(&all, "Shop MariaDB TLS").password.is_none());
    assert_eq!(backend.other_app_reads.lock().unwrap().len(), 3);
}

#[test]
fn on_linux_keychain_passwords_are_not_read_and_the_preview_says_so() {
    let all = read_as(WorkbenchPasswords::SecretService);
    for name in ["Shop MySQL", "Shop MariaDB TLS", "Orders via bastion"] {
        let c = named(&all, name);
        assert!(c.stored_password.is_none(), "{name}");
        assert!(
            c.unmapped
                .iter()
                .any(|u| u.starts_with("Password: ") && u.contains("does not read")),
            "{name}: {:?}",
            c.unmapped
        );
    }
}

#[test]
fn every_connection_becomes_a_connection_file_and_the_bastion_a_network_profile() {
    let fx = Fixture::new("workbench-import");
    let backend = FakeBackend::default();
    let folder = TargetFolder {
        folder: "Workbench".into(),
        keep_client_folders: true,
    };
    let mut imported = 0;
    for connection in read() {
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
    assert_eq!(imported, 3);
    assert_eq!(fx.config().connections.len(), 3);
    let shop = fx.read("user/connections/workbench/shop-mysql.toml");
    assert!(shop.contains("database = \"spelaion\""), "{shop}");
    let tls = fx.read("user/connections/workbench/prod/shop-mariadb-tls.toml");
    assert!(
        tls.contains("ca.crt") && tls.contains("client.key") && tls.contains("verify-ca"),
        "{tls}"
    );
    let bastion = fx.read("user/connections/workbench/prod/orders-via-bastion.toml");
    assert!(
        bastion.contains("tunnel = \"spelaion-127-0-0-1\""),
        "{bastion}"
    );
    let tunnel = fx.read("user/tunnels/spelaion-127-0-0-1.toml");
    assert!(tunnel.contains("id_ed25519"), "{tunnel}");
    // No password reached a file or the keychain.
    assert!(backend.keychain.lock().unwrap().is_empty());
}

// ------------------------------------------------------------- files no fixture covers

/// A `connections.xml` with one connection of Workbench's shape and the given settings.
fn one_connection(driver: &str, settings: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<data grt_format="2.0">
  <value _ptr_="0" type="list" content-type="object" content-struct-name="db.mgmt.Connection">
    <value type="object" struct-name="db.mgmt.Connection" id="{{X}}" struct-checksum="0x96ba47d8">
      <link type="object" struct-name="db.mgmt.Driver" key="driver">{driver}</link>
      <value type="string" key="hostIdentifier">Mysql@db.example:3306</value>
      <value _ptr_="1" type="dict" key="parameterValues">
{settings}
      </value>
      <value type="string" key="name">Crafted</value>
    </value>
  </value>
</data>"#
    )
}

fn read_text(fx: &Fixture, text: &str) -> Result<Vec<ImportedConnection>, WorkbenchError> {
    let path = fx.write("Workbench/connections.xml", text);
    read_workbench(&path, WorkbenchPasswords::Vault)
}

#[test]
fn a_server_workbench_saw_as_mariadb_is_imported_as_mariadb() {
    let fx = Fixture::new("workbench-mariadb");
    let all = read_text(
        &fx,
        &one_connection(
            "com.mysql.rdbms.mysql.driver.native",
            r#"<value type="string" key="hostName">db.example</value>
               <value type="string" key="serverVersion">10.11.6-MariaDB-1:10.11.6+maria~ubu2204</value>
               <value type="int" key="lastConnected">1794229000</value>"#,
        ),
    )
    .unwrap();
    assert_eq!(all[0].engine, Ok(Engine::Mariadb));
    // Nothing to say about what Workbench learnt when connecting; no port means the default.
    assert!(all[0].unmapped.is_empty(), "{:?}", all[0].unmapped);
    assert_eq!(all[0].port, None);
}

#[test]
fn secret_looking_options_show_their_name_only_and_passwords_in_the_file_are_secrets() {
    let fx = Fixture::new("workbench-secrets");
    let all = read_text(
        &fx,
        &one_connection(
            "com.mysql.rdbms.mysql.driver.native_sshtun",
            r#"<value type="string" key="hostName">db.example</value>
               <value type="string" key="userName">app</value>
               <value type="string" key="password">old-plain-pw</value>
               <value type="string" key="sshHost">[2001:db8::1]:2200</value>
               <value type="string" key="sshUserName">ops</value>
               <value type="string" key="sshPassword">old-ssh-pw</value>
               <value type="int" key="sshCompressionLevel">6</value>
               <value type="string" key="sslCipher">AES256-SHA</value>
               <value type="string" key="OPT_AUTH_TOKEN">tok-secret</value>
               <value type="int" key="CLIENT_COMPRESS">0</value>"#,
        ),
    )
    .unwrap();
    let c = &all[0];
    assert_eq!(
        c.password.as_ref().map(|p| p.expose().to_string()),
        Some("old-plain-pw".into())
    );
    let ssh = c.ssh.clone().unwrap();
    assert_eq!((ssh.host.as_str(), ssh.port), ("2001:db8::1", Some(2200)));
    assert_eq!(ssh.auth, SshAuth::Password);
    assert_eq!(
        ssh.password.as_ref().map(|p| p.expose().to_string()),
        Some("old-ssh-pw".into())
    );
    assert!(
        c.unmapped
            .contains(&"Option: OPT_AUTH_TOKEN (value not shown)".to_string()),
        "{:?}",
        c.unmapped
    );
    assert!(c.unmapped.contains(&"SSH: compression level 6".to_string()));
    assert!(c.unmapped.contains(&"TLS: ciphers AES256-SHA".to_string()));
    // An unticked box says nothing; no secret is ever shown.
    assert!(!c.unmapped.iter().any(|u| u.contains("CLIENT_COMPRESS")));
    for secret in ["old-plain-pw", "old-ssh-pw", "tok-secret"] {
        assert!(
            !c.unmapped.iter().any(|u| u.contains(secret)),
            "{secret}: {:?}",
            c.unmapped
        );
    }
}

#[test]
fn unknown_methods_are_listed_and_broken_files_are_refused() {
    let fx = Fixture::new("workbench-broken");
    let all = read_text(
        &fx,
        &one_connection("com.mysql.rdbms.mysql.driver.fabric", ""),
    )
    .unwrap();
    assert!(
        all[0].engine.clone().unwrap_err().contains("fabric"),
        "{:?}",
        all[0].engine
    );
    assert!(matches!(
        read_text(&fx, "<data><value"),
        Err(WorkbenchError::Unreadable { .. })
    ));
    assert!(matches!(
        read_text(&fx, "<?xml version=\"1.0\"?><other/>"),
        Err(WorkbenchError::Unreadable { .. })
    ));
    // A folder without Workbench's file says where it was looked for.
    let error = read_workbench(&fx.base.join("nowhere"), WorkbenchPasswords::Vault).unwrap_err();
    assert!(matches!(error, WorkbenchError::NotFound(_)));
    assert!(error.to_string().contains("connections.xml"), "{error}");
}
