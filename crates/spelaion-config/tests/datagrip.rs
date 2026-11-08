//! DataGrip's import (`datagrip.rs`) against files a real DataGrip 2026.3 wrote
//! (`tests/fixtures/datagrip`, see its README): every supported engine, an SSH tunnel through
//! one of the IDE's SSH configurations, TLS with certificate files, and a password saved in
//! DataGrip's KeePass database, which is never read.

mod common;

use std::path::PathBuf;

use common::{FakeBackend, Fixture};
use spelaion_config::{
    Engine, ImportedConnection, Source, SshAuth, TargetFolder, TlsModeConfig, add_connection,
    add_tunnel, import_plan, read_datagrip,
};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/datagrip")
}

fn project() -> PathBuf {
    fixtures().join("DataGripProjects/default")
}

fn config_dir() -> PathBuf {
    fixtures().join("JetBrains/DataGrip2026.3")
}

fn read() -> Vec<ImportedConnection> {
    read_datagrip(&project(), &[config_dir()]).unwrap()
}

fn named<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
    all.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not found"))
}

const KEEPASS: &str = "Password: if DataGrip saved one, it is in its KeePass database, which SpelaionDB does not read; it is asked when connecting";

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
            assert!(c.password.is_none(), "{name}");
            assert!(c.folder.is_empty() && c.description.is_empty(), "{name}");
            assert!(!c.read_only, "{name}");
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

    // The same project read from its `.idea` folder gives the same connections.
    let from_idea = read_datagrip(&project().join(".idea"), &[config_dir()]).unwrap();
    assert_eq!(from_idea.len(), all.len());
    // Ids are DataGrip's, so a second read finds the same ones.
    assert_eq!(
        named(&all, "Orders PG").id,
        "e2d2bd51-007a-4680-865b-0a13f2bdddc8"
    );
}

#[test]
fn tls_comes_with_its_certificate_files() {
    let all = read();
    let tls = named(&all, "Orders PG").tls.clone().unwrap();
    assert_eq!(tls.mode, TlsModeConfig::Require);
    assert_eq!(
        tls.ca,
        "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\ca.crt"
    );
    assert_eq!(
        tls.cert,
        "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\client.crt"
    );
    assert_eq!(
        tls.key,
        "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\client.key"
    );
    for name in [
        "Shop MySQL",
        "Reports MSSQL",
        "Local file",
        "PG via bastion",
    ] {
        assert!(named(&all, name).tls.is_none(), "{name}");
    }
}

#[test]
fn the_ssh_tunnel_comes_from_the_ides_ssh_configuration() {
    let all = read();
    let bastion = named(&all, "PG via bastion");
    let ssh = bastion.ssh.clone().unwrap();
    assert_eq!(ssh.host, "127.0.0.1");
    assert_eq!(ssh.port, Some(2221));
    assert_eq!(ssh.user, "spelaion");
    // DataGrip writes no `authType` for a key pair.
    assert_eq!(ssh.auth, SshAuth::Key);
    assert_eq!(
        ssh.key_path,
        "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\id_ed25519"
    );
    assert!(ssh.password.is_none());
    assert!(
        bastion.unmapped.iter().any(|u| u.contains("~/.ssh/config")),
        "{:?}",
        bastion.unmapped
    );
    assert!(all.iter().filter(|c| c.ssh.is_some()).count() == 1);

    // Without the IDE's configuration folder the tunnel cannot be found, and the preview says so.
    let alone = read_datagrip(&project(), &[]).unwrap();
    let bastion = named(&alone, "PG via bastion");
    assert!(bastion.ssh.is_none());
    assert!(
        bastion
            .unmapped
            .iter()
            .any(|u| u.contains("SSH configuration was not found")),
        "{:?}",
        bastion.unmapped
    );
}

#[test]
fn a_password_in_datagrips_keepass_is_not_read_and_the_preview_says_so() {
    let all = read();
    // `security.xml` was never written: DataGrip's default, KeePass on Windows, the system
    // keychain elsewhere.
    let orders = named(&all, "Orders PG");
    if cfg!(target_os = "macos") {
        let item = orders.stored_password.clone().unwrap();
        assert_eq!(
            item.service,
            "IntelliJ Platform DB — e2d2bd51-007a-4680-865b-0a13f2bdddc8"
        );
        assert_eq!(item.account, "spelaion");
    } else {
        assert!(orders.stored_password.is_none());
        assert!(
            orders.unmapped.iter().any(|u| u.starts_with("Password: ")),
            "{:?}",
            orders.unmapped
        );
    }
    if cfg!(windows) {
        assert_eq!(orders.unmapped, [KEEPASS]);
    }
    // "Never" saves nothing, and files have no password at all.
    for name in ["Reports MSSQL", "Local file", "Analytics"] {
        let c = named(&all, name);
        assert!(c.stored_password.is_none(), "{name}");
        assert!(
            !c.unmapped.iter().any(|u| u.starts_with("Password: ")),
            "{name}: {:?}",
            c.unmapped
        );
    }
    // Nothing else in these files is left behind.
    for name in [
        "Shop MySQL",
        "Shop MariaDB",
        "Reports MSSQL",
        "Local file",
        "Analytics",
    ] {
        let c = named(&all, name);
        assert!(
            c.unmapped.iter().all(|u| u.starts_with("Password: ")),
            "{name}: {:?}",
            c.unmapped
        );
    }
}

#[test]
fn every_connection_becomes_a_connection_file_and_the_bastion_a_network_profile() {
    let fx = Fixture::new("datagrip-import");
    let backend = FakeBackend::default();
    let folder = TargetFolder {
        folder: "DataGrip".into(),
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
    let bastion = fx.read("user/connections/datagrip/pg-via-bastion.toml");
    assert!(
        bastion.contains("tunnel = \"spelaion-127-0-0-1\""),
        "{bastion}"
    );
    let tunnel = fx.read("user/tunnels/spelaion-127-0-0-1.toml");
    assert!(tunnel.contains("id_ed25519"), "{tunnel}");
    let orders = fx.read("user/connections/datagrip/orders-pg.toml");
    assert!(
        orders.contains("ca.crt") && orders.contains("client.key"),
        "{orders}"
    );
    // No password reached a file or the keychain: DataGrip's are in its KeePass database.
    assert!(backend.keychain.lock().unwrap().is_empty());
}
