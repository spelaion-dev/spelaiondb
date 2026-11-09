//! pgAdmin's import (`pgadmin.rs`) against what a real pgAdmin 4 9.19 for Windows wrote
//! (`tests/fixtures/pgadmin`, see its README): the JSON its Tools → Import/Export Servers →
//! Export wrote, and the server tables of its own `pgadmin4.db`. Two server groups, a plain
//! server, one with TLS (verify-ca, certificate files), a role, a comment and a tag, one through
//! an SSH tunnel with an identity file, and one that names a pg_service.conf service
//! SpelaionDB cannot open. pgAdmin exports no passwords, and the ones it saves in its database
//! are never read.

mod common;

use std::path::{Path, PathBuf};

use common::{FakeBackend, Fixture};
use spelaion_config::{
    Engine, ImportedConnection, PgadminError, Source, SshAuth, TargetFolder, TlsModeConfig,
    add_connection, add_tunnel, import_plan, read_pgadmin,
};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pgadmin")
}

fn export_file() -> PathBuf {
    fixtures().join("pgadmin-9.19-servers.json")
}

fn read_export() -> Vec<ImportedConnection> {
    read_pgadmin(&export_file()).unwrap()
}

/// pgAdmin's folder with a `pgadmin4.db` holding the fixture's tables, plus `extra` SQL.
fn database(fx: &Fixture, extra: &str) -> PathBuf {
    let dir = fx.base.join("pgAdmin");
    std::fs::create_dir_all(&dir).unwrap();
    let db = rusqlite::Connection::open(dir.join("pgadmin4.db")).unwrap();
    let sql = std::fs::read_to_string(fixtures().join("pgadmin4-9.19.sql")).unwrap();
    // The fixture leaves out pgAdmin's `user` table, which its servers point to.
    db.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    db.execute_batch(&sql).unwrap();
    db.execute_batch(extra).unwrap();
    dir
}

fn named<'a>(all: &'a [ImportedConnection], name: &str) -> &'a ImportedConnection {
    all.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not found"))
}

const KEYS: &str = "C:\\Users\\tester\\spelaiondb\\testing\\docker\\keys\\";

const NO_PASSWORD: &str =
    "Password: pgAdmin does not export passwords; it is asked when connecting";
const PASSPHRASE: &str = "SSH: if the private key has a passphrase, it is asked when connecting";

#[test]
fn every_server_comes_with_its_address_user_and_maintenance_database() {
    let all = read_export();
    let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Local PG",
            "Orders TLS",
            "PG via bastion",
            "Analytics service"
        ]
    );
    let expect = |name: &str, host: &str, port: u16| {
        let c = named(&all, name);
        assert_eq!(c.engine, Ok(Engine::Postgres), "{name}");
        assert_eq!(c.host, host, "{name}");
        assert_eq!(c.port, Some(port), "{name}");
        assert_eq!(c.database, "spelaion", "{name}");
        assert_eq!(c.user, "spelaion", "{name}");
        assert!(
            c.password.is_none() && c.stored_password.is_none() && !c.has_password(),
            "{name}"
        );
        assert!(
            c.env.is_empty() && !c.read_only && c.proxy.is_none(),
            "{name}"
        );
    };
    expect("Local PG", "127.0.0.1", 15432);
    expect("Orders TLS", "127.0.0.1", 15432);
    // Through the tunnel, the database's address as the bastion sees it.
    expect("PG via bastion", "postgres", 5432);
    assert_eq!(named(&all, "Local PG").id, "1");
    assert_eq!(named(&all, "PG via bastion").id, "3");
}

#[test]
fn a_pg_service_server_is_listed_but_cannot_be_imported() {
    let all = read_export();
    let service = named(&all, "Analytics service");
    let reason = service.engine.clone().unwrap_err();
    assert!(
        reason.contains("pg_service.conf") && reason.contains("analytics"),
        "{reason}"
    );
    assert!(service.host.is_empty() && service.port.is_none());
    assert!(service.tls.is_none() && service.ssh.is_none());
    // Nothing to say about a password it will never be imported with.
    assert!(service.unmapped.is_empty(), "{:?}", service.unmapped);
}

#[test]
fn server_groups_become_folders() {
    let all = read_export();
    assert_eq!(named(&all, "Local PG").folder, "Servers");
    assert_eq!(named(&all, "Analytics service").folder, "Servers");
    assert_eq!(named(&all, "Orders TLS").folder, "Production");
    assert_eq!(named(&all, "PG via bastion").folder, "Production");
}

#[test]
fn tls_comes_with_its_mode_and_certificate_files() {
    let all = read_export();
    let tls = named(&all, "Orders TLS").tls.clone().unwrap();
    assert_eq!(tls.mode, TlsModeConfig::VerifyCa);
    assert_eq!(tls.ca, format!("{KEYS}ca.crt"));
    assert_eq!(tls.cert, format!("{KEYS}client.crt"));
    assert_eq!(tls.key, format!("{KEYS}client.key"));
    // pgAdmin's default, prefer, is SpelaionDB's.
    assert!(named(&all, "Local PG").tls.is_none());
    assert!(named(&all, "PG via bastion").tls.is_none());
}

#[test]
fn the_comment_and_tags_come_along() {
    let all = read_export();
    let orders = named(&all, "Orders TLS");
    assert_eq!(orders.description, "Orders of the EU shop");
    assert_eq!(orders.tags, ["eu"]);
    assert!(named(&all, "Local PG").description.is_empty());
    assert!(named(&all, "Local PG").tags.is_empty());
}

#[test]
fn the_ssh_tunnel_becomes_a_one_hop_tunnel_with_its_identity_file() {
    let all = read_export();
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
    // The other servers say "no tunnel" with pgAdmin's defaults (port 22, password).
    assert_eq!(all.iter().filter(|c| c.ssh.is_some()).count(), 1);
}

#[test]
fn what_is_not_imported_is_listed_and_every_password_is_asked() {
    let all = read_export();
    assert_eq!(named(&all, "Local PG").unmapped, [NO_PASSWORD]);
    assert_eq!(
        named(&all, "Orders TLS").unmapped,
        [
            NO_PASSWORD,
            "Role: reporting (add SET ROLE to the connection's session setup)",
            "Tag colours: eu #3BAA6C",
        ]
    );
    assert_eq!(
        named(&all, "PG via bastion").unmapped,
        [PASSPHRASE, NO_PASSWORD]
    );
}

#[test]
fn pgadmins_own_database_holds_the_same_servers() {
    let fx = Fixture::new("pgadmin-db");
    let dir = database(&fx, "");
    let from_db = read_pgadmin(&dir).unwrap();
    // The database file itself reads the same as pgAdmin's folder.
    assert_eq!(
        read_pgadmin(&dir.join("pgadmin4.db")).unwrap().len(),
        from_db.len()
    );
    let exported = read_export();
    assert_eq!(from_db.len(), exported.len());
    for (db, ex) in from_db.iter().zip(&exported) {
        let name = &ex.name;
        assert_eq!(db.name, ex.name);
        assert_eq!(db.id, ex.id, "{name}");
        assert_eq!(db.folder, ex.folder, "{name}");
        assert_eq!(db.engine, ex.engine, "{name}");
        assert_eq!(
            (&db.host, db.port, &db.database, &db.user),
            (&ex.host, ex.port, &ex.database, &ex.user),
            "{name}"
        );
        assert_eq!(db.tls, ex.tls, "{name}");
        assert_eq!(db.description, ex.description, "{name}");
        assert_eq!(db.tags, ex.tags, "{name}");
        assert_eq!(
            db.ssh.as_ref().map(|s| (
                &s.host,
                s.port,
                &s.user,
                s.auth,
                &s.key_path,
                s.keepalive_interval
            )),
            ex.ssh.as_ref().map(|s| (
                &s.host,
                s.port,
                &s.user,
                s.auth,
                &s.key_path,
                s.keepalive_interval
            )),
            "{name}"
        );
        // Nothing saved in pgAdmin: nothing to say about passwords. The rest is the same.
        let without_export_note: Vec<&String> =
            ex.unmapped.iter().filter(|u| *u != NO_PASSWORD).collect();
        assert_eq!(db.unmapped.iter().collect::<Vec<_>>(), without_export_note);
    }
}

#[test]
fn passwords_pgadmin_saved_are_never_read_and_the_preview_says_so() {
    let fx = Fixture::new("pgadmin-saved");
    let dir = database(
        &fx,
        "UPDATE server SET save_password = 1, password = 'enc-db-secret' WHERE name = 'Local PG';
         INSERT INTO server (user_id, servergroup_id, name, host, port, maintenance_db, username,
             use_ssh_tunnel, tunnel_host, tunnel_port, tunnel_username, tunnel_authentication,
             tunnel_password, connection_params, tags)
           VALUES (1, 2, 'Password tunnel', 'db.internal', 5432, 'app', 'app', 1, 'jump.example',
             '22', 'ops', 0, 'enc-ssh-secret', '{\"sslmode\": \"prefer\"}', '[]');
         INSERT INTO server (user_id, servergroup_id, name, host, port, is_adhoc, password,
             save_password)
           VALUES (1, 1, 'Query tool one-off', 'adhoc.example', 5432, 1, 'enc-adhoc', 1);",
    );
    let all = read_pgadmin(&dir).unwrap();
    // The query tool's ad hoc connections are not pgAdmin's servers.
    assert!(!all.iter().any(|c| c.name == "Query tool one-off"));
    let local = named(&all, "Local PG");
    assert!(local.password.is_none() && local.stored_password.is_none());
    assert!(
        local
            .unmapped
            .iter()
            .any(|u| u.starts_with("Password: pgAdmin saved one") && u.contains("does not read")),
        "{:?}",
        local.unmapped
    );
    let tunnel = named(&all, "Password tunnel");
    let ssh = tunnel.ssh.clone().unwrap();
    assert_eq!(ssh.auth, SshAuth::Password);
    assert!(ssh.password.is_none() && ssh.key_path.is_empty());
    assert_eq!((ssh.host.as_str(), ssh.port), ("jump.example", Some(22)));
    assert!(
        tunnel
            .unmapped
            .iter()
            .any(|u| u.starts_with("SSH: pgAdmin saved the SSH password")),
        "{:?}",
        tunnel.unmapped
    );
    // No saved secret reaches anything this reader returns.
    let everything = format!("{all:?}");
    for secret in ["enc-db-secret", "enc-ssh-secret", "enc-adhoc"] {
        assert!(!everything.contains(secret), "{secret}");
    }
}

#[test]
fn every_server_becomes_a_connection_file_and_the_bastion_a_network_profile() {
    let fx = Fixture::new("pgadmin-import");
    let backend = FakeBackend::default();
    let folder = TargetFolder {
        folder: "pgAdmin".into(),
        keep_client_folders: true,
    };
    let mut imported = 0;
    for connection in read_export() {
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
    let local = fx.read("user/connections/pgadmin/servers/local-pg.toml");
    assert!(
        local.contains("port = 15432") && local.contains("database = \"spelaion\""),
        "{local}"
    );
    let orders = fx.read("user/connections/pgadmin/production/orders-tls.toml");
    assert!(
        orders.contains("verify-ca") && orders.contains("ca.crt") && orders.contains("client.key"),
        "{orders}"
    );
    assert!(orders.contains("Orders of the EU shop"), "{orders}");
    assert!(orders.contains("\"eu\""), "{orders}");
    let bastion = fx.read("user/connections/pgadmin/production/pg-via-bastion.toml");
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

fn read_text(fx: &Fixture, text: &str) -> Result<Vec<ImportedConnection>, PgadminError> {
    let path = fx.write("export/servers.json", text);
    read_pgadmin(&path)
}

#[test]
fn unknown_settings_are_listed_and_secret_ones_show_their_name_only() {
    let fx = Fixture::new("pgadmin-unknown");
    let all = read_text(
        &fx,
        r##"{"Servers": {"1": {
            "Name": "Crafted", "Group": "Lab", "Host": "db.example", "Port": "6543",
            "MaintenanceDB": "app", "Username": "app",
            "BGColor": "#FF0000", "Shared": true, "SharedUsername": "team",
            "KerberosAuthentication": true,
            "PasswordExecCommand": "vault read -field=pw secret/db",
            "PostConnectionSQL": "SET search_path TO app",
            "DBRestriction": "app,reports",
            "PrepareThreshold": 5,
            "FutureSetting": "x", "FutureToken": "tok-secret",
            "ConnectionParameters": {
                "sslmode": "allow", "connect_timeout": 30,
                "application_name": "pgAdmin", "sslpassword": "key-pass-secret",
                "passfile": "C:\\pg\\pgpass.conf", "sslcompression": 0
            }
        }}}"##,
    )
    .unwrap();
    let c = &all[0];
    assert_eq!(c.port, Some(6543));
    assert!(c.tls.is_none());
    for expected in [
        "TLS: SSL mode allow (SpelaionDB's default, prefer, is kept)",
        "Kerberos authentication (SpelaionDB signs in with a password)",
        "Password exec command (not run; the password is asked when connecting)",
        "Password file: C:\\pg\\pgpass.conf (not read; the password is asked when connecting)",
        "Post-connection SQL: SET search_path TO app (add it to the connection's session setup)",
        "Shown databases limited to: app,reports",
        "Colour: background #FF0000",
        "Shared with other pgAdmin users (as team)",
        "Connection timeout: 30 s",
        "Connection parameter: application_name=pgAdmin",
        "Connection parameter: sslpassword (value not shown)",
        "Option: FutureSetting=x",
        "Option: FutureToken (value not shown)",
        "Option: PrepareThreshold=5",
    ] {
        assert!(
            c.unmapped.iter().any(|u| u == expected),
            "{expected}: {:?}",
            c.unmapped
        );
    }
    // An unset parameter says nothing; no secret is ever shown.
    assert!(!c.unmapped.iter().any(|u| u.contains("sslcompression")));
    for secret in ["tok-secret", "key-pass-secret", "vault read"] {
        assert!(
            !c.unmapped.iter().any(|u| u.contains(secret)),
            "{secret}: {:?}",
            c.unmapped
        );
    }
}

#[test]
fn servers_keep_pgadmins_order_and_older_exports_tls_keys_are_read() {
    let fx = Fixture::new("pgadmin-order");
    let server = |name: &str| {
        format!(r#"{{"Name": "{name}", "Group": "Servers", "Host": "h", "Port": 5432}}"#)
    };
    let mut entries: Vec<String> = (1..=11)
        .map(|i| format!("\"{i}\": {}", server(&format!("S{i}"))))
        .collect();
    entries.push(
        r#""12": {"Name": "Old", "Host": "old.example", "Port": 5432, "SSLMode": "require",
                  "SSLRootCert": "/certs/root.crt"}"#
            .into(),
    );
    let all = read_text(&fx, &format!(r#"{{"Servers": {{{}}}}}"#, entries.join(","))).unwrap();
    let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[..3], ["S1", "S2", "S3"]);
    assert_eq!(names[10..], ["S11", "Old"]);
    let old = named(&all, "Old");
    let tls = old.tls.clone().unwrap();
    assert_eq!(tls.mode, TlsModeConfig::Require);
    assert_eq!(tls.ca, "/certs/root.crt");
    assert!(old.folder.is_empty());
}

#[test]
fn socket_hosts_and_servers_without_a_host_are_not_imported() {
    let fx = Fixture::new("pgadmin-socket");
    let all = read_text(
        &fx,
        r#"{"Servers": {
            "1": {"Name": "Socket", "Host": "/var/run/postgresql", "Port": 5432},
            "2": {"Name": "Nothing", "Port": 5432}
        }}"#,
    )
    .unwrap();
    let socket = named(&all, "Socket").engine.clone().unwrap_err();
    assert!(socket.contains("local socket"), "{socket}");
    let nothing = named(&all, "Nothing").engine.clone().unwrap_err();
    assert!(nothing.contains("without a host"), "{nothing}");
}

#[test]
fn files_that_are_not_pgadmins_are_refused() {
    let fx = Fixture::new("pgadmin-bad");
    for text in ["", "not json", "{\"servers\": {}}", "[1, 2]"] {
        let error = read_text(&fx, text).unwrap_err();
        assert!(
            matches!(error, PgadminError::Unreadable { .. }),
            "{text}: {error}"
        );
    }
    // Another SQLite database is not pgAdmin's.
    let other = fx.base.join("other.db");
    rusqlite::Connection::open(&other)
        .unwrap()
        .execute_batch("CREATE TABLE t (x)")
        .unwrap();
    assert!(matches!(
        read_pgadmin(&other),
        Err(PgadminError::Unreadable { .. })
    ));
    // A folder without pgadmin4.db.
    let empty = fx.base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(matches!(
        read_pgadmin(&empty),
        Err(PgadminError::NotFound(_))
    ));
    assert!(matches!(
        read_pgadmin(Path::new("C:/does/not/exist.json")),
        Err(PgadminError::NotFound(_))
    ));
}

#[test]
fn the_database_is_never_changed() {
    let fx = Fixture::new("pgadmin-readonly");
    let dir = database(&fx, "");
    let file = dir.join("pgadmin4.db");
    let before = std::fs::read(&file).unwrap();
    read_pgadmin(&dir).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    // Nothing next to it either (no journal or WAL left behind).
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["pgadmin4.db"]);
}
