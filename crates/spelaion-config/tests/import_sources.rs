//! The import's list of sources (SDB-446): what each client has, counted by the core from the
//! repository's fixtures, in each state the list shows; and that counting reads no password the
//! clients saved (a credentials file or a key that would break a full read is not even opened).

use std::path::PathBuf;

use crate::common::Fixture;
use spelaion_config::{
    ImportClient, ImportPlaces, ImportReadError, ImportSourceState, SavedSecrets, import_source,
    read_datagrip, read_heidisql, read_import, read_pgadmin, read_tableplus, read_workbench,
};

fn fixtures(client: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(client)
}

/// A DBeaver workspace with two connections, and `credentials` as its saved credentials file.
fn dbeaver(fx: &Fixture, credentials: &[u8]) -> PathBuf {
    let dir = fx.base.join("DBeaverData/workspace6/General/.dbeaver");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("data-sources.json"),
        r#"{"connections": {
            "postgres-jdbc-1": {"provider": "postgresql", "driver": "postgres-jdbc", "name": "Shop",
                "configuration": {"host": "db", "port": "5432", "database": "shop", "user": "app"}},
            "sqlite-1": {"provider": "sqlite", "driver": "sqlite_jdbc", "name": "Local",
                "configuration": {"database": "/tmp/local.db"}}
        }}"#,
    )
    .unwrap();
    std::fs::write(dir.join("credentials-config.json"), credentials).unwrap();
    fx.base.join("DBeaverData/workspace6")
}

fn found(state: &ImportSourceState) -> usize {
    match state {
        ImportSourceState::Found { count, .. } => *count,
        other => panic!("expected connections found, got {other:?}"),
    }
}

#[test]
fn clients_with_connections_are_counted_where_they_keep_them() {
    let _turn = crate::serial::file_turn(module_path!());
    let tableplus = fixtures("tableplus").join("com.tinyapp.TablePlus");
    let workbench = fixtures("workbench").join("Workbench");
    let heidisql = fixtures("heidisql").join("heidisql-12.22.txt");
    let jetbrains = fixtures("datagrip").join("JetBrains");
    let pgadmin = fixtures("pgadmin").join("pgadmin-9.19-servers.json");
    let places = ImportPlaces::none()
        .with(ImportClient::Tableplus, &tableplus)
        .with(ImportClient::Workbench, &workbench)
        .with(ImportClient::Heidisql, &heidisql)
        .with(ImportClient::Datagrip, &jetbrains)
        .with(ImportClient::Pgadmin, &pgadmin);

    // The same counts the readers give when the client is picked.
    let expected = [
        (
            ImportClient::Tableplus,
            read_tableplus(&tableplus, None).unwrap().len(),
        ),
        (
            ImportClient::Workbench,
            read_workbench(
                &workbench,
                spelaion_config::WorkbenchPasswords::this_system(),
            )
            .unwrap()
            .len(),
        ),
        (
            ImportClient::Heidisql,
            read_heidisql(&heidisql).unwrap().len(),
        ),
        (ImportClient::Pgadmin, read_pgadmin(&pgadmin).unwrap().len()),
    ];
    for (client, count) in expected {
        assert!(count > 0, "{client:?}'s fixture has connections");
        let state = import_source(&places, client, None);
        assert_eq!(found(&state), count, "{client:?}");
        if let ImportSourceState::Found { place, .. } = state {
            assert!(!place.is_empty(), "{client:?} says where it read");
        }
    }

    // A project folder chosen for DataGrip, with its IDE configuration from JetBrains' folder.
    let project = fixtures("datagrip").join("DataGripProjects/default");
    let config = spelaion_config::datagrip_dirs(&jetbrains);
    let state = import_source(&places, ImportClient::Datagrip, Some(&project));
    assert_eq!(
        found(&state),
        read_datagrip(&project, &config).unwrap().len()
    );
}

#[test]
fn a_client_with_nothing_where_it_keeps_connections_is_not_found() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("sources-none");
    let empty = fx.base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    for client in spelaion_config::IMPORT_CLIENTS {
        let missing = if client == ImportClient::Heidisql {
            empty.join("settings.txt")
        } else {
            empty.clone()
        };
        let places = ImportPlaces::none().with(client, &missing);
        assert_eq!(
            import_source(&places, client, None),
            ImportSourceState::NotFound,
            "{client:?}"
        );
        // Still readable from a folder or file chosen on its row.
        assert!(places.has_default(client));
    }
}

#[test]
fn heidisql_without_its_registry_or_an_export_needs_a_file() {
    let _turn = crate::serial::file_turn(module_path!());
    let places = ImportPlaces::none();
    assert_eq!(
        import_source(&places, ImportClient::Heidisql, None),
        ImportSourceState::NeedsFile
    );
    assert!(!places.has_default(ImportClient::Heidisql));
    // A chosen export file is read as before.
    let file = fixtures("heidisql").join("heidisql-12.22.txt");
    assert!(found(&import_source(&places, ImportClient::Heidisql, Some(&file))) > 0);
}

#[test]
fn an_encrypted_export_is_not_opened_to_count_it() {
    let _turn = crate::serial::file_turn(module_path!());
    let export = fixtures("tableplus").join("export.tableplusconnection");
    let places = ImportPlaces::none().with(ImportClient::Tableplus, &export);
    assert_eq!(
        import_source(&places, ImportClient::Tableplus, None),
        ImportSourceState::Encrypted {
            place: export.display().to_string()
        }
    );
}

#[test]
fn a_client_that_fails_says_why_on_its_own_row() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("sources-failed");
    let dir = fx.base.join("General/.dbeaver");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("data-sources.json"), "{ not json").unwrap();
    let places = ImportPlaces::none()
        .with(ImportClient::Dbeaver, &fx.base)
        .with(
            ImportClient::Workbench,
            fixtures("workbench").join("Workbench"),
        );
    match import_source(&places, ImportClient::Dbeaver, None) {
        ImportSourceState::Failed { reason } => {
            assert!(reason.contains("data-sources.json"), "{reason}")
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    // The others are not held by it.
    assert!(found(&import_source(&places, ImportClient::Workbench, None)) > 0);
}

/// Counting never reads a password a client saved in its files: a DBeaver credentials file that
/// a full read cannot decrypt, and HeidiSQL passwords that a full read cannot decode, change
/// nothing in the list, because the list never opens or decodes them.
#[test]
fn listing_reads_no_saved_password() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("sources-secrets");
    let workspace = dbeaver(&fx, b"this is not DBeaver's encrypted credentials file");
    let places = ImportPlaces::none().with(ImportClient::Dbeaver, &workspace);
    // A full read opens the credentials file, and cannot decrypt it.
    assert!(matches!(
        read_import(&places, ImportClient::Dbeaver, None, None, SavedSecrets::Read),
        Err(ImportReadError::Other(reason)) if reason.contains("credentials")
    ));
    // The list does not open it.
    assert_eq!(
        found(&import_source(&places, ImportClient::Dbeaver, None)),
        2
    );
    let skipped = read_import(
        &places,
        ImportClient::Dbeaver,
        None,
        None,
        SavedSecrets::Skip,
    )
    .unwrap();
    assert!(skipped.connections.iter().all(|c| c.password.is_none()));

    // HeidiSQL: undecodable passwords are reported by a full read, never even decoded by the list.
    let settings = fx.base.join("heidisql.txt");
    std::fs::write(
        &settings,
        "Servers\\Shop\\Host<|||>1<|||>db\r\nServers\\Shop\\NetType<|||>3<|||>0\r\nServers\\Shop\\Password<|||>1<|||>é not hex\r\n",
    )
    .unwrap();
    let places = ImportPlaces::none().with(ImportClient::Heidisql, &settings);
    let read = read_import(
        &places,
        ImportClient::Heidisql,
        None,
        None,
        SavedSecrets::Read,
    )
    .unwrap();
    let skipped = read_import(
        &places,
        ImportClient::Heidisql,
        None,
        None,
        SavedSecrets::Skip,
    )
    .unwrap();
    assert_eq!(read.connections.len(), 1);
    assert!(
        read.connections[0]
            .unmapped
            .iter()
            .any(|line| line.starts_with("Password:")),
        "{:?}",
        read.connections[0].unmapped
    );
    assert!(
        skipped.connections[0]
            .unmapped
            .iter()
            .all(|line| !line.starts_with("Password:"))
    );
    assert!(skipped.connections[0].password.is_none());
    assert_eq!(
        found(&import_source(&places, ImportClient::Heidisql, None)),
        1
    );
}

/// The list's state of every client, as the page reads it: one state each, whatever the others.
#[test]
fn every_client_has_one_state() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("sources-all");
    let workspace = dbeaver(&fx, b"");
    let places = ImportPlaces::none()
        .with(ImportClient::Dbeaver, &workspace)
        .with(
            ImportClient::Tableplus,
            fixtures("tableplus").join("export.tableplusconnection"),
        )
        .with(ImportClient::Workbench, fx.base.join("no-workbench"));
    let states: Vec<(ImportClient, ImportSourceState)> = spelaion_config::IMPORT_CLIENTS
        .into_iter()
        .map(|client| (client, import_source(&places, client, None)))
        .collect();
    assert_eq!(states.len(), 7);
    let state = |client| &states.iter().find(|(c, _)| *c == client).unwrap().1;
    assert_eq!(found(state(ImportClient::Dbeaver)), 2);
    assert!(matches!(
        state(ImportClient::Tableplus),
        ImportSourceState::Encrypted { .. }
    ));
    assert_eq!(state(ImportClient::Workbench), &ImportSourceState::NotFound);
    assert_eq!(state(ImportClient::Heidisql), &ImportSourceState::NeedsFile);
    // No place given: the others need one chosen on their rows.
    for client in [
        ImportClient::Datagrip,
        ImportClient::Pgadmin,
        ImportClient::Beekeeper,
    ] {
        assert_eq!(state(client), &ImportSourceState::NeedsFile, "{client:?}");
    }
}
