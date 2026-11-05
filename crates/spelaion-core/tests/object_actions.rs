//! The explorer's object actions (ADR-026) run on every engine: SQLite and DuckDB always,
//! PostgreSQL, MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without
//! them they are skipped, which does not count as tested).

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, ObjectChange, ObjectError, ObjectTarget, ResultStore, Session, apply_change,
    object_actions,
};
use spelaion_driver_api::{ConnectParams, Driver, ObjectKind, Secret, TlsMode};

async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

/// Every engine available here, with the schema its objects go in.
async fn engines() -> Vec<(&'static str, Session, &'static str)> {
    let memory = || ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let mut out = vec![
        (
            "sqlite",
            session(&spelaion_driver_sqlite::SqliteDriver::default(), memory()).await,
            "main",
        ),
        (
            "duckdb",
            session(&spelaion_driver_duckdb::DuckdbDriver::default(), memory()).await,
            "main",
        ),
    ];
    let login =
        |host: String, port: u16, user: &str, password: &str, database: &str| ConnectParams {
            host,
            port,
            user: user.into(),
            password: Some(Secret::new(password)),
            database: database.into(),
            ..Default::default()
        };
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        let params = login(host, port, "spelaion", "spelaion", "spelaion");
        out.push((
            "postgres",
            session(&spelaion_driver_postgres::PostgresDriver, params).await,
            "public",
        ));
    }
    for (label, var, driver) in [
        (
            "mysql",
            "SPELAION_TEST_MYSQL",
            spelaion_driver_mysql::MysqlDriver::mysql(),
        ),
        (
            "mariadb",
            "SPELAION_TEST_MARIADB",
            spelaion_driver_mysql::MysqlDriver::mariadb(),
        ),
    ] {
        if let Some((host, port)) = server(var) {
            let params = login(host, port, "spelaion", "spelaion", "spelaion");
            out.push((label, session(&driver, params).await, "spelaion"));
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        // tempdb always exists: no other test has to create a database first.
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push((
            "sqlserver",
            session(&spelaion_driver_sqlserver::SqlServerDriver, params).await,
            "dbo",
        ));
    }
    out
}

/// Runs each statement on its own (a T-SQL `CREATE VIEW` must be alone in its batch); panics
/// on the first failure unless `tolerate` (for cleanups of what may not exist).
async fn run(label: &str, session: &Session, statements: &[&str], tolerate: bool) {
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            match event {
                CoreEvent::StatementFailed { error, .. } if !tolerate => {
                    panic!("{label}: {statement}\n→ {}", error.message)
                }
                CoreEvent::ResultStarted { result, .. } => {
                    session.results().remove(*result);
                }
                _ => {}
            }
        }
    }
}

async fn relations(session: &Session, schema: &str) -> Vec<String> {
    let mut names: Vec<String> = session
        .connection()
        .list_objects(schema)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.name)
        .collect();
    names.sort();
    names
}

async fn columns(session: &Session, schema: &str, table: &str) -> Vec<String> {
    session
        .connection()
        .describe_table(schema, table)
        .await
        .unwrap_or_else(|e| panic!("{table}: {e}"))
        .columns
        .into_iter()
        .map(|c| c.name)
        .collect()
}

fn rename(target: ObjectTarget, new_name: &str) -> ObjectChange {
    ObjectChange::Rename {
        target,
        new_name: new_name.into(),
    }
}

fn relation(schema: &str, name: &str, kind: ObjectKind) -> ObjectTarget {
    ObjectTarget::Relation {
        schema: schema.into(),
        name: name.into(),
        kind,
    }
}

#[tokio::test]
async fn rename_tables_views_and_columns() {
    for (label, session, schema) in engines().await {
        let cleanup = [
            "DROP VIEW oa_rename_v",
            "DROP VIEW oa_renamed_v",
            "DROP TABLE oa_rename_t",
            "DROP TABLE oa_renamed_t",
        ];
        run(label, &session, &cleanup, true).await;
        run(
            label,
            &session,
            &[
                "CREATE TABLE oa_rename_t (id INT PRIMARY KEY, a INT)",
                // Not on the table: DuckDB refuses to rename what a view depends on.
                "CREATE VIEW oa_rename_v AS SELECT 1 AS id",
            ],
            false,
        )
        .await;

        let column = ObjectTarget::Column {
            schema: schema.into(),
            table: "oa_rename_t".into(),
            name: "a".into(),
        };
        apply_change(&session, &rename(column, "b"), false, true)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(
            columns(&session, schema, "oa_rename_t").await,
            ["id", "b"],
            "{label}"
        );

        let table = relation(schema, "oa_rename_t", ObjectKind::Table);
        apply_change(&session, &rename(table, "oa_renamed_t"), false, true)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let view = relation(schema, "oa_rename_v", ObjectKind::View);
        if object_actions(session.dialect().as_ref()).rename_views {
            apply_change(&session, &rename(view, "oa_renamed_v"), false, true)
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
        } else {
            assert!(matches!(
                apply_change(&session, &rename(view, "oa_renamed_v"), false, true).await,
                Err(ObjectError::Unsupported(_))
            ));
        }
        let names = relations(&session, schema).await;
        assert!(
            names.contains(&"oa_renamed_t".to_string()),
            "{label}: {names:?}"
        );
        assert!(
            !names.contains(&"oa_rename_t".to_string()),
            "{label}: {names:?}"
        );
        let renamed_view = if label == "sqlite" {
            "oa_rename_v"
        } else {
            "oa_renamed_v"
        };
        assert!(
            names.contains(&renamed_view.to_string()),
            "{label}: {names:?}"
        );

        run(label, &session, &cleanup, true).await;
    }
}

#[tokio::test]
async fn changes_run_only_confirmed_and_never_on_read_only_connections() {
    let session = session(
        &spelaion_driver_sqlite::SqliteDriver::default(),
        ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        },
    )
    .await;
    run("sqlite", &session, &["CREATE TABLE kept (id INT)"], false).await;
    let change = rename(relation("main", "kept", ObjectKind::Table), "gone");
    assert_eq!(
        apply_change(&session, &change, true, true).await,
        Err(ObjectError::ReadOnly)
    );
    assert_eq!(
        apply_change(&session, &change, false, false).await,
        Err(ObjectError::NeedsConfirmation)
    );
    assert_eq!(relations(&session, "main").await, ["kept"]);
    // A failing statement says which one, and what the engine answered.
    let missing = rename(relation("main", "missing", ObjectKind::Table), "other");
    match apply_change(&session, &missing, false, true).await {
        Err(ObjectError::Failed {
            index: 1,
            total: 1,
            message,
        }) => {
            assert!(message.contains("missing"), "{message}")
        }
        other => panic!("{other:?}"),
    }
}
