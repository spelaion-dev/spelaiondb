//! Another client's startup SQL, imported as the connection's session setup (SDB-104,
//! ADR-037). TablePlus's fixture (`spelaion-config`'s `tests/fixtures/tableplus`) brings
//! `SET search_path TO app, public;` on Orders PG, a Production connection. The core checks
//! an imported setup as the connection form checks a typed one: where the read-only barrier
//! applies, a statement that could write is taken out and listed, never imported silently and
//! never run. No server is needed: nothing runs.

use std::path::PathBuf;

use spelaion_config::{Config, ConnectionTypes, ImportedConnection, read_tableplus};
use spelaion_core::{check_session_setup, vet_imported_setup};

fn tableplus() -> Vec<ImportedConnection> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../spelaion-config/tests/fixtures/tableplus/com.tinyapp.TablePlus");
    read_tableplus(&dir, None).expect("the TablePlus fixture")
}

fn orders() -> ImportedConnection {
    tableplus()
        .into_iter()
        .find(|c| c.name == "Orders PG")
        .expect("Orders PG")
}

/// The built-in connection types, with no repository or personal file.
fn built_in() -> ConnectionTypes {
    ConnectionTypes::of_repository(&Config::default())
}

/// Vets `connection`'s setup with the built-in type its `env` names.
fn vet(connection: &mut ImportedConnection) {
    let kind = built_in().resolve(Some(connection.env.as_str()));
    vet_imported_setup(connection, &kind);
}

/// The setup as it would run: every statement passes the read-only barrier.
fn passes_the_barrier(setup: &str) -> bool {
    let dialect = spelaion_dialect::for_engine("postgres").expect("a dialect");
    check_session_setup(dialect.as_ref(), dialect.name(), setup, true).is_ok()
}

#[test]
fn tableplus_bootstrap_commands_become_the_session_setup() {
    let _turn = crate::serial::file_turn(module_path!());
    let mut orders = orders();
    assert_eq!(orders.env, "prod");
    let kind = built_in().resolve(Some("prod")).unwrap().unwrap();
    assert!(kind.safeguards.confirm_data_changes);
    let unmapped = orders.unmapped.clone();
    vet(&mut orders);
    assert_eq!(orders.session.setup, "SET search_path TO app, public;");
    assert_eq!(orders.unmapped, unmapped, "nothing is taken out");
    assert!(passes_the_barrier(&orders.session.setup));
}

#[test]
fn a_write_cannot_be_sneaked_through_a_production_setup() {
    let _turn = crate::serial::file_turn(module_path!());
    for setup in [
        "SET search_path TO app, public; DELETE FROM orders",
        "SET search_path TO app, public;\nUPDATE orders SET total = 0;",
        "SET default_transaction_read_only = off; SET search_path TO app",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        "COMMIT; DROP TABLE orders",
        "SET search_path TO app /* ; */; INSERT INTO orders VALUES (1)",
        "SET search_path TO app; DO $$ BEGIN DELETE FROM orders; END $$",
        "SET search_path TO app; CALL purge_orders()",
        "SET search_path TO app; SELECT 1; TRUNCATE orders",
        "SET search_path TO app; COPY orders FROM PROGRAM 'rm -rf /'",
        "SET search_path = (SELECT nextval('s')::text)",
        "WITH d AS (DELETE FROM orders RETURNING *) SELECT 1",
    ] {
        let mut orders = orders();
        orders.session.setup = setup.to_string();
        let before = orders.unmapped.len();
        vet(&mut orders);
        let listed = &orders.unmapped[before..];
        assert!(!listed.is_empty(), "{setup}: nothing was taken out");
        for line in listed {
            assert!(
                line.starts_with("Session setup: `") && line.contains("(not imported: "),
                "{setup}: {line}"
            );
        }
        // What is left runs on a Production connection, and holds no write.
        assert!(
            passes_the_barrier(&orders.session.setup),
            "{setup} → {:?}",
            orders.session.setup
        );
        let kept = orders.session.setup.to_ascii_lowercase();
        for write in [
            "delete",
            "update",
            "insert",
            "drop",
            "truncate",
            "call",
            "copy",
            "read write",
        ] {
            assert!(!kept.contains(write), "{setup} → {kept:?}");
        }
    }
}

#[test]
fn a_read_only_connection_is_checked_too_and_others_take_the_setup_whole() {
    let _turn = crate::serial::file_turn(module_path!());
    let setup = "SET search_path TO app; DELETE FROM audit";
    // Read-only, without a type.
    let mut connection = orders();
    connection.env.clear();
    connection.read_only = true;
    connection.session.setup = setup.into();
    vet(&mut connection);
    assert_eq!(connection.session.setup, "SET search_path TO app;");
    let last = connection.unmapped.last().unwrap();
    assert!(
        last.starts_with("Session setup: `DELETE FROM audit` (not imported: "),
        "{last}"
    );
    // A development connection: the setup is the user's own, and comes whole.
    let mut connection = orders();
    connection.env = "dev".into();
    connection.session.setup = setup.into();
    let before = connection.unmapped.clone();
    vet(&mut connection);
    assert_eq!(connection.session.setup, setup);
    assert_eq!(connection.unmapped, before);
    // Every statement taken out: no setup, and "ignore errors" goes with it.
    let mut connection = orders();
    connection.session.setup = "DELETE FROM a; DELETE FROM b".into();
    connection.session.ignore_setup_errors = true;
    vet(&mut connection);
    assert_eq!(connection.session.setup, "");
    assert!(!connection.session.ignore_setup_errors);
    assert_eq!(
        connection
            .unmapped
            .iter()
            .filter(|l| l.starts_with("Session setup:"))
            .count(),
        2
    );
}

#[test]
fn a_setup_that_cannot_be_checked_is_not_imported() {
    let _turn = crate::serial::file_turn(module_path!());
    // An engine SpelaionDB does not know on a Production connection: fail closed.
    let mut connection = orders();
    connection.engine = Err("oracle/oracle_thin is not supported yet".into());
    connection.session.setup = "SET search_path TO app".into();
    vet(&mut connection);
    assert_eq!(connection.session.setup, "");
    let last = connection.unmapped.last().unwrap();
    assert!(
        last.contains("its engine is unknown, so its statements cannot be checked"),
        "{last}"
    );
}

#[test]
fn what_is_kept_stays_as_written() {
    let _turn = crate::serial::file_turn(module_path!());
    // A statement that already ends in `;`, a trailing comment after a kept one, a refused one
    // between them: cutting it out never runs two kept statements together.
    let mut orders = orders();
    orders.session.setup = "SET search_path TO app; -- the app's tables first\n\
        DELETE FROM orders;\n\
        SET timezone TO 'UTC' -- trailing comment\n\
        ;\n\
        SET statement_timeout = 5000"
        .into();
    vet(&mut orders);
    let kept = orders.session.setup.clone();
    assert!(passes_the_barrier(&kept), "{kept:?}");
    let dialect = spelaion_dialect::for_engine("postgres").unwrap();
    let statements: Vec<String> = dialect
        .split_statements(&kept)
        .iter()
        .map(|span| span.text(&kept).to_string())
        .collect();
    assert_eq!(statements.len(), 3, "{statements:?}");
    assert!(statements[0].starts_with("SET search_path TO app"));
    assert!(statements[1].contains("SET timezone TO 'UTC'"));
    assert!(statements[2].starts_with("SET statement_timeout = 5000"));
    assert!(!kept.to_ascii_lowercase().contains("delete"), "{kept:?}");
    // A kept statement's comment stays; the one before the refused statement went with it.
    assert!(
        kept.contains("-- trailing comment"),
        "comments stay: {kept:?}"
    );

    // SQL Server's `GO` batches on a read-only connection.
    let mut reports = tableplus()
        .into_iter()
        .find(|c| c.name == "Reports MSSQL")
        .expect("Reports MSSQL");
    reports.read_only = true;
    reports.session.setup =
        "SET NOCOUNT ON\nGO\nDELETE FROM dbo.audit\nGO\nSET LANGUAGE us_english\nGO".into();
    vet(&mut reports);
    let kept = reports.session.setup.clone();
    let dialect = spelaion_dialect::for_engine("sqlserver").unwrap();
    assert!(
        check_session_setup(dialect.as_ref(), dialect.name(), &kept, true).is_ok(),
        "{kept:?}"
    );
    let statements: Vec<String> = dialect
        .split_statements(&kept)
        .iter()
        .map(|span| span.text(&kept).to_string())
        .collect();
    assert_eq!(
        statements,
        ["SET NOCOUNT ON", "SET LANGUAGE us_english"],
        "{kept:?}"
    );
    assert!(
        reports
            .unmapped
            .iter()
            .any(|l| l.starts_with("Session setup: `DELETE FROM dbo.audit`")),
        "{:?}",
        reports.unmapped
    );
}
