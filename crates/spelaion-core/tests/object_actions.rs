//! The explorer's object actions (ADR-026) run on every engine: SQLite and DuckDB always,
//! PostgreSQL, MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without
//! them they are skipped, which does not count as tested).

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, ObjectChange, ObjectError, ObjectTarget, ResultStore, Session, apply_change,
    object_actions,
};
use spelaion_driver_api::{
    ConnectParams, ConstraintKind, Driver, ObjectKind, SchemaObject, SchemaObjectKind, Secret,
    TlsMode,
};

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

fn drop(target: ObjectTarget) -> ObjectChange {
    ObjectChange::Drop {
        target,
        cascade: false,
    }
}

async fn applied(label: &str, session: &Session, change: ObjectChange) {
    apply_change(session, &change, false, true)
        .await
        .unwrap_or_else(|e| panic!("{label}: {change:?}: {e}"));
}

/// An object each engine has in a schema, to drop: its creation and how it is listed.
fn schema_object(label: &str) -> Option<(&'static [&'static str], &'static str, SchemaObjectKind)> {
    Some(match label {
        "postgres" => (
            &["CREATE FUNCTION oa_drop_f(a integer) RETURNS integer LANGUAGE sql AS 'SELECT a'"],
            "oa_drop_f",
            SchemaObjectKind::Function,
        ),
        "mysql" | "mariadb" => (
            &["CREATE PROCEDURE oa_drop_p() SELECT 1"],
            "oa_drop_p",
            SchemaObjectKind::Procedure,
        ),
        "sqlserver" => (
            &["CREATE SEQUENCE oa_drop_seq"],
            "oa_drop_seq",
            SchemaObjectKind::Sequence,
        ),
        "duckdb" => (
            &["CREATE MACRO oa_drop_m(x) AS x * 2"],
            "oa_drop_m",
            SchemaObjectKind::Function,
        ),
        "sqlite" => (
            &["CREATE TRIGGER oa_drop_trg AFTER INSERT ON oa_drop_t BEGIN SELECT 1; END"],
            "oa_drop_trg",
            SchemaObjectKind::Trigger,
        ),
        _ => return None,
    })
}

#[tokio::test]
async fn drop_what_the_explorer_lists() {
    for (label, session, schema) in engines().await {
        let cleanup = [
            "DROP VIEW oa_drop_v",
            "DROP TABLE oa_drop_child",
            "DROP TABLE oa_drop_t",
            "DROP FUNCTION oa_drop_f",
            "DROP PROCEDURE oa_drop_p",
            "DROP SEQUENCE oa_drop_seq",
            "DROP MACRO oa_drop_m",
        ];
        run(label, &session, &cleanup, true).await;
        run(
            label,
            &session,
            &[
                "CREATE TABLE oa_drop_t (id INT PRIMARY KEY, a INT, b INT)",
                "CREATE INDEX oa_drop_idx ON oa_drop_t (a)",
                "CREATE VIEW oa_drop_v AS SELECT 1 AS id",
            ],
            false,
        )
        .await;
        let target = |name: &str| {
            (
                schema.to_string(),
                "oa_drop_t".to_string(),
                name.to_string(),
            )
        };

        let (schema_name, table, name) = target("oa_drop_idx");
        let index = ObjectTarget::Index {
            schema: schema_name,
            table,
            name,
            primary: false,
        };
        applied(label, &session, drop(index)).await;
        let info = session
            .connection()
            .describe_table(schema, "oa_drop_t")
            .await
            .unwrap();
        assert!(
            info.indexes.iter().all(|i| i.name != "oa_drop_idx"),
            "{label}: {:?}",
            info.indexes
        );

        // After the index: DuckDB alters no table that an index depends on.
        let (schema_name, table, name) = target("b");
        applied(
            label,
            &session,
            drop(ObjectTarget::Column {
                schema: schema_name,
                table,
                name,
            }),
        )
        .await;
        assert_eq!(
            columns(&session, schema, "oa_drop_t").await,
            ["id", "a"],
            "{label}"
        );

        // The primary key's index goes with its constraint, where the engine lists one.
        let constraints = object_actions(session.dialect().as_ref()).drop_constraints;
        if let Some(primary) = info.indexes.iter().find(|i| i.primary && constraints) {
            let (schema_name, table, name) = target(&primary.name);
            let index = ObjectTarget::Index {
                schema: schema_name,
                table,
                name,
                primary: true,
            };
            applied(label, &session, drop(index)).await;
            let info = session
                .connection()
                .describe_table(schema, "oa_drop_t")
                .await
                .unwrap();
            assert!(
                info.primary_key.is_empty(),
                "{label}: {:?}",
                info.primary_key
            );
            run(
                label,
                &session,
                &["ALTER TABLE oa_drop_t ADD PRIMARY KEY (id)"],
                false,
            )
            .await;
        }

        if object_actions(session.dialect().as_ref()).drop_constraints {
            run(
                label,
                &session,
                &[
                    "CREATE TABLE oa_drop_child (id INT PRIMARY KEY, parent INT, \
                   CONSTRAINT oa_drop_fk FOREIGN KEY (parent) REFERENCES oa_drop_t (id), \
                   CONSTRAINT oa_drop_chk CHECK (parent > 0), \
                   CONSTRAINT oa_drop_uq UNIQUE (parent))",
                ],
                false,
            )
            .await;
            let child = |name: &str| {
                (
                    schema.to_string(),
                    "oa_drop_child".to_string(),
                    name.to_string(),
                )
            };
            let (schema_name, table, name) = child("oa_drop_fk");
            applied(
                label,
                &session,
                drop(ObjectTarget::ForeignKey {
                    schema: schema_name,
                    table,
                    name,
                }),
            )
            .await;
            for (name, kind) in [
                ("oa_drop_chk", ConstraintKind::Check),
                ("oa_drop_uq", ConstraintKind::Unique),
            ] {
                let (schema_name, table, name) = child(name);
                applied(
                    label,
                    &session,
                    drop(ObjectTarget::Constraint {
                        schema: schema_name,
                        table,
                        name,
                        kind,
                    }),
                )
                .await;
            }
            let info = session
                .connection()
                .describe_table(schema, "oa_drop_child")
                .await
                .unwrap();
            assert!(
                info.foreign_keys.is_empty(),
                "{label}: {:?}",
                info.foreign_keys
            );
            assert!(
                info.constraints
                    .iter()
                    .all(|c| !matches!(c.kind, ConstraintKind::Check | ConstraintKind::Unique)),
                "{label}: {:?}",
                info.constraints
            );
            let child = relation(schema, "oa_drop_child", ObjectKind::Table);
            applied(label, &session, drop(child)).await;
        }

        if let Some((create, name, kind)) = schema_object(label) {
            run(label, &session, create, false).await;
            let listed = session
                .connection()
                .list_schema_objects(schema)
                .await
                .unwrap();
            let object = listed
                .iter()
                .find(|o| o.name == name && o.kind == kind)
                .unwrap_or_else(|| panic!("{label}: {name} in {listed:?}"))
                .clone();
            // A detail the catalog did not write is refused before anything runs.
            let forged = SchemaObject {
                detail: Some("integer) CASCADE; DROP TABLE oa_drop_t; --".into()),
                ..object.clone()
            };
            assert!(
                matches!(
                    apply_change(
                        &session,
                        &drop(ObjectTarget::SchemaObject(forged)),
                        false,
                        true
                    )
                    .await,
                    Err(ObjectError::Gone(_))
                ),
                "{label}"
            );
            applied(label, &session, drop(ObjectTarget::SchemaObject(object))).await;
            let listed = session
                .connection()
                .list_schema_objects(schema)
                .await
                .unwrap();
            assert!(listed.iter().all(|o| o.name != name), "{label}: {listed:?}");
        }

        applied(
            label,
            &session,
            drop(relation(schema, "oa_drop_v", ObjectKind::View)),
        )
        .await;
        applied(
            label,
            &session,
            drop(relation(schema, "oa_drop_t", ObjectKind::Table)),
        )
        .await;
        let names = relations(&session, schema).await;
        assert!(
            names.iter().all(|n| !n.starts_with("oa_drop")),
            "{label}: {names:?}"
        );
        run(label, &session, &cleanup, true).await;
    }
}

/// A session that may create and drop databases: MySQL and MariaDB's test user cannot.
async fn admin_session(label: &str, session: Session) -> Session {
    let (var, driver) = match label {
        "mysql" => (
            "SPELAION_TEST_MYSQL",
            spelaion_driver_mysql::MysqlDriver::mysql(),
        ),
        "mariadb" => (
            "SPELAION_TEST_MARIADB",
            spelaion_driver_mysql::MysqlDriver::mariadb(),
        ),
        _ => return session,
    };
    let (host, port) = server(var).expect("set");
    let params = ConnectParams {
        host,
        port,
        user: "root".into(),
        password: Some(Secret::new("spelaion-root")),
        database: "spelaion".into(),
        ..Default::default()
    };
    self::session(&driver, params).await
}

#[tokio::test]
async fn schemas_drop_and_cascade_where_the_engine_can() {
    for (label, session, _) in engines().await {
        if !object_actions(session.dialect().as_ref()).drop_schemas {
            continue;
        }
        let session = admin_session(label, session).await;
        let mysql = matches!(label, "mysql" | "mariadb");
        let create = if mysql {
            "CREATE DATABASE oa_drop_s"
        } else {
            "CREATE SCHEMA oa_drop_s"
        };
        let cleanup = [
            "DROP TABLE oa_drop_s.t",
            if mysql {
                "DROP DATABASE oa_drop_s"
            } else {
                "DROP SCHEMA oa_drop_s"
            },
        ];
        run(label, &session, &cleanup, true).await;
        run(
            label,
            &session,
            &[create, "CREATE TABLE oa_drop_s.t (id INT)"],
            false,
        )
        .await;
        let schema = ObjectTarget::Schema {
            name: "oa_drop_s".into(),
        };
        let cascades = spelaion_core::can_cascade(session.dialect().as_ref(), &schema);
        if cascades {
            // Not empty: without CASCADE the engine refuses, and nothing is dropped.
            assert!(
                matches!(
                    apply_change(&session, &drop(schema.clone()), false, true).await,
                    Err(ObjectError::Failed { index: 1, .. })
                ),
                "{label}"
            );
            let change = ObjectChange::Drop {
                target: schema,
                cascade: true,
            };
            applied(label, &session, change).await;
        } else {
            if !mysql {
                // SQL Server drops only an empty schema, and has no CASCADE.
                run(label, &session, &["DROP TABLE oa_drop_s.t"], false).await;
            }
            applied(label, &session, drop(schema)).await;
        }
        let schemas = session.connection().list_schemas().await.unwrap();
        assert!(schemas.iter().all(|s| s.name != "oa_drop_s"), "{label}");
    }
}
