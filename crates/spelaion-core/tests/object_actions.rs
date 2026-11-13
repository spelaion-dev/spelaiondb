//! The explorer's object actions (ADR-026) run on every engine: SQLite and DuckDB always,
//! PostgreSQL, MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without
//! them they are skipped, which does not count as tested).

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    ColumnEdit, CoreEvent, CreateKind, DroppedConstraint, EditableColumn, FoundObject, NewColumn,
    NewForeignKey, NewIndex, NewIndexKind, NewTable, NewTableElement, ObjectChange, ObjectError,
    ObjectTarget, ResultStore, SEARCH_LIMIT, Session, SourceTarget, TableAlteration,
    TableStructure, active_schema, alteration_plan, apply_alteration, apply_change,
    apply_create_table, create_sql, create_table_plan, editable_source, new_index_sql,
    new_table_element_sql, object_actions, search_objects, set_active_schema,
    supports_added_checks, table_structure,
};
use spelaion_driver_api::{
    ConnectParams, ConstraintKind, Driver, NameFilter, ObjectKind, SchemaObject, SchemaObjectKind,
    Secret, TlsMode, Value,
};

async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
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

#[tokio::test]
async fn what_create_starts_runs_on_each_engine() {
    for (label, session, schema) in engines().await {
        let session = admin_session(label, session).await;
        let dialect = session.dialect().clone();
        for kind in object_actions(dialect.as_ref()).create {
            let (name, cleanup) = match kind {
                CreateKind::Table => ("new_table", "DROP TABLE new_table"),
                CreateKind::View => ("new_view", "DROP VIEW new_view"),
                CreateKind::MaterializedView => (
                    "new_materialized_view",
                    "DROP MATERIALIZED VIEW new_materialized_view",
                ),
                CreateKind::Sequence => ("new_sequence", "DROP SEQUENCE new_sequence"),
                CreateKind::Schema if matches!(label, "mysql" | "mariadb") => {
                    ("new_database", "DROP DATABASE new_database")
                }
                CreateKind::Schema => ("new_schema", "DROP SCHEMA new_schema"),
            };
            run(label, &session, &[cleanup], true).await;
            let sql = create_sql(dialect.as_ref(), kind, schema);
            run(
                label,
                &session,
                &[sql.trim_end().trim_end_matches(';')],
                false,
            )
            .await;
            let found = match kind {
                CreateKind::Table | CreateKind::View | CreateKind::MaterializedView => {
                    relations(&session, schema)
                        .await
                        .contains(&name.to_string())
                }
                CreateKind::Sequence => session
                    .connection()
                    .list_schema_objects(schema)
                    .await
                    .unwrap()
                    .iter()
                    .any(|o| o.name == name),
                CreateKind::Schema => session
                    .connection()
                    .list_schemas()
                    .await
                    .unwrap()
                    .iter()
                    .any(|s| s.name == name),
            };
            assert!(found, "{label}: {sql}");
            run(label, &session, &[cleanup], false).await;
        }
    }
}

#[tokio::test]
async fn indexes_and_constraints_on_chosen_columns_run_on_each_engine() {
    for (label, session, schema) in engines().await {
        let dialect = session.dialect().clone();
        for kind in object_actions(dialect.as_ref()).new_index {
            run(label, &session, &["DROP TABLE oa_index_t"], true).await;
            run(
                label,
                &session,
                &["CREATE TABLE oa_index_t (id INT NOT NULL, a INT, b INT)"],
                false,
            )
            .await;
            let columns: Vec<String> = match kind {
                NewIndexKind::PrimaryKey => vec!["id".into()],
                _ => vec!["b".into(), "a".into()],
            };
            let sql =
                new_index_sql(dialect.as_ref(), schema, "oa_index_t", &columns, kind).unwrap();
            run(
                label,
                &session,
                &[sql.trim_end().trim_end_matches(';')],
                false,
            )
            .await;
            let info = session
                .connection()
                .describe_table(schema, "oa_index_t")
                .await
                .unwrap();
            match kind {
                NewIndexKind::Index | NewIndexKind::UniqueIndex => {
                    let name = format!(
                        "oa_index_t_b_a_{}",
                        if kind == NewIndexKind::Index {
                            "idx"
                        } else {
                            "key"
                        }
                    );
                    let index = info.indexes.iter().find(|i| i.name == name);
                    let index =
                        index.unwrap_or_else(|| panic!("{label}: {name} in {:?}", info.indexes));
                    assert_eq!(index.unique, kind == NewIndexKind::UniqueIndex, "{label}");
                }
                NewIndexKind::UniqueConstraint => assert!(
                    info.constraints
                        .iter()
                        .any(|c| c.kind == ConstraintKind::Unique && c.columns == columns),
                    "{label}: {:?}",
                    info.constraints
                ),
                NewIndexKind::PrimaryKey => assert_eq!(info.primary_key, columns, "{label}"),
            }
        }
        run(label, &session, &["DROP TABLE oa_index_t"], true).await;
    }
}

#[tokio::test]
async fn new_table_elements_run_on_supported_engines() {
    for (label, session, schema) in engines().await {
        let dialect = session.dialect().clone();
        let capabilities = object_actions(dialect.as_ref());
        let version = session.connection().server_version().await.unwrap();
        run(
            label,
            &session,
            &["DROP TABLE oa_new_child", "DROP TABLE oa_new_parent"],
            true,
        )
        .await;
        if label == "postgres" {
            run(
                label,
                &session,
                &["DROP FUNCTION oa_new_trigger_fn()"],
                true,
            )
            .await;
        }
        run(
            label,
            &session,
            &[
                "CREATE TABLE oa_new_parent (id INT PRIMARY KEY)",
                "CREATE TABLE oa_new_child (id INT, parent_id INT)",
            ],
            false,
        )
        .await;
        let mut elements = vec![
            NewTableElement::Column {
                name: "extra".into(),
                data_type: "INTEGER".into(),
                nullable: true,
                default: "3".into(),
            },
            NewTableElement::Index {
                name: "oa new idx".into(),
                columns: vec!["extra".into()],
                unique: false,
            },
        ];
        if capabilities.add_constraints {
            elements.push(NewTableElement::Constraint {
                name: "oa_new_unique".into(),
                kind: "unique".into(),
                columns: vec!["id".into()],
                expression: String::new(),
            });
            if supports_added_checks(dialect.as_ref(), &version) {
                elements.push(NewTableElement::Constraint {
                    name: "oa_new_check".into(),
                    kind: "check".into(),
                    columns: vec![],
                    expression: "extra >= 0".into(),
                });
            }
            elements.push(NewTableElement::ForeignKey {
                name: "oa_new_fk".into(),
                columns: vec!["parent_id".into()],
                referenced_schema: schema.into(),
                referenced_table: "oa_new_parent".into(),
                referenced_columns: vec!["id".into()],
            });
        }
        if capabilities.add_triggers {
            let body = match label {
                "postgres" => {
                    run(label, &session, &["CREATE FUNCTION oa_new_trigger_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$"], false).await;
                    "oa_new_trigger_fn"
                }
                "mysql" | "mariadb" => {
                    "BEGIN SET @oa_trigger_seen = 1; SET @oa_trigger_other = 2; END"
                }
                "sqlserver" => "SET NOCOUNT ON",
                _ => "SELECT 1",
            };
            elements.push(NewTableElement::Trigger {
                name: "oa_new_trigger".into(),
                timing: "after".into(),
                event: "insert".into(),
                body: body.into(),
            });
        }
        for element in &elements {
            let sql =
                new_table_element_sql(dialect.as_ref(), &version, schema, "oa_new_child", element)
                    .unwrap();
            run(label, &session, &[&sql], false).await;
        }
        let info = session
            .connection()
            .describe_table(schema, "oa_new_child")
            .await
            .unwrap();
        assert!(info.columns.iter().any(|c| c.name == "extra"), "{label}");
        assert!(
            info.indexes.iter().any(|i| i.name == "oa new idx"),
            "{label}"
        );
        if capabilities.add_constraints {
            assert!(
                info.foreign_keys.iter().any(|fk| fk.name == "oa_new_fk"),
                "{label}"
            );
            assert!(
                info.constraints
                    .iter()
                    .any(|c| c.name.as_deref() == Some("oa_new_unique")),
                "{label}"
            );
        }
        if capabilities.add_triggers {
            let objects = session
                .connection()
                .list_schema_objects(schema)
                .await
                .unwrap();
            assert!(
                objects.iter().any(|o| o.name == "oa_new_trigger"),
                "{label}"
            );
        }
        run(
            label,
            &session,
            &["DROP TABLE oa_new_child", "DROP TABLE oa_new_parent"],
            false,
        )
        .await;
        if label == "postgres" {
            run(
                label,
                &session,
                &["DROP FUNCTION oa_new_trigger_fn()"],
                false,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn open_an_object_by_name_on_each_engine() {
    for (label, session, schema) in engines().await {
        let cleanup = [
            "DROP VIEW oa_find_order_view",
            "DROP TABLE oa_find_orders",
            "DROP TABLE oa_find_ord",
        ];
        run(label, &session, &cleanup, true).await;
        run(
            label,
            &session,
            &[
                "CREATE TABLE oa_find_orders (id INT)",
                "CREATE TABLE oa_find_ord (id INT)",
                "CREATE VIEW oa_find_order_view AS SELECT 1 AS id",
            ],
            false,
        )
        .await;
        let none = NameFilter::default();
        let connection = session.connection().as_ref();
        let found = search_objects(connection, "OA_FIND_ORD", &none, &none, false, SEARCH_LIMIT)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let names: Vec<&str> = found.iter().map(FoundObject::name).collect();
        // The exact name first, then the others in order; nothing else.
        assert_eq!(
            names,
            ["oa_find_ord", "oa_find_order_view", "oa_find_orders"],
            "{label}"
        );
        assert!(found.iter().all(|o| o.schema() == schema), "{label}");
        assert!(
            matches!(&found[1], FoundObject::Relation(o) if o.kind == ObjectKind::View),
            "{label}"
        );
        // The explorer's filter applies, and the limit.
        let hide = NameFilter {
            include: Vec::new(),
            exclude: vec!["*view".into()],
        };
        let found = search_objects(connection, "oa_find_ord", &none, &hide, false, 1)
            .await
            .unwrap();
        assert_eq!(
            found.iter().map(FoundObject::name).collect::<Vec<_>>(),
            ["oa_find_ord"],
            "{label}"
        );
        let elsewhere = NameFilter {
            include: vec!["no_such_schema".into()],
            exclude: Vec::new(),
        };
        assert!(
            search_objects(
                connection,
                "oa_find",
                &elsewhere,
                &none,
                false,
                SEARCH_LIMIT
            )
            .await
            .unwrap()
            .is_empty(),
            "{label}"
        );
        run(label, &session, &cleanup, true).await;
    }
}

#[tokio::test]
async fn the_editors_schema_is_set_where_the_engine_has_one_per_session() {
    for (label, session, default_schema) in engines().await {
        let session = admin_session(label, session).await;
        if !object_actions(session.dialect().as_ref()).active_schema {
            assert!(matches!(
                set_active_schema(&session, default_schema).await,
                Err(ObjectError::Unsupported(_))
            ));
            assert_eq!(
                active_schema(&session).await.unwrap().as_deref(),
                Some(default_schema),
                "{label}"
            );
            continue;
        }
        let mysql = matches!(label, "mysql" | "mariadb");
        let cleanup = [if mysql {
            "DROP DATABASE oa_active"
        } else {
            "DROP SCHEMA oa_active CASCADE"
        }];
        run(label, &session, &cleanup, true).await;
        let create = if mysql {
            "CREATE DATABASE oa_active"
        } else {
            "CREATE SCHEMA oa_active"
        };
        run(
            label,
            &session,
            &[create, "CREATE TABLE oa_active.oa_only_here (id INT)"],
            false,
        )
        .await;
        set_active_schema(&session, "oa_active")
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(
            active_schema(&session).await.unwrap().as_deref(),
            Some("oa_active"),
            "{label}"
        );
        // Unqualified names are found there now.
        run(
            label,
            &session,
            &["SELECT count(*) FROM oa_only_here"],
            false,
        )
        .await;
        set_active_schema(&session, default_schema).await.unwrap();
        assert_eq!(
            active_schema(&session).await.unwrap().as_deref(),
            Some(default_schema),
            "{label}"
        );
        run(label, &session, &cleanup, true).await;
    }
}

async fn scalar(session: &Session, sql: &str) -> Value {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{sql}: {events:?}"));
    let value = {
        let stored = session.results().get(result).unwrap();
        let stored = stored.read().unwrap();
        stored.window(0, 1).first().map_or(Value::Null, |batch| {
            Value::from_array(
                batch.column(0).as_ref(),
                0,
                stored.schema().columns[0].logical,
            )
            .unwrap_or(Value::Null)
        })
    };
    session.results().remove(result);
    value
}

async fn text(session: &Session, sql: &str) -> String {
    match scalar(session, sql).await {
        Value::Text(text) => text,
        other => other.to_string(),
    }
}

fn edit_of(column: &EditableColumn) -> ColumnEdit {
    ColumnEdit {
        name: column.name.clone(),
        data_type: column.data_type.clone(),
        nullable: column.nullable,
        default: column.default.clone(),
        comment: column.comment.clone(),
    }
}

async fn structure(label: &str, session: &Session, schema: &str, table: &str) -> TableStructure {
    table_structure(session, schema, table)
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"))
}

fn column<'a>(structure: &'a TableStructure, name: &str) -> &'a EditableColumn {
    structure
        .columns
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name}: {structure:?}"))
}

/// Applies a form's change as the dialog does: preview, then apply what was shown, confirmed.
async fn apply_form(label: &str, session: &Session, alteration: &TableAlteration) -> String {
    let plan = alteration_plan(session, alteration)
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
    let script = plan.script();
    apply_alteration(
        session,
        alteration,
        &script,
        false,
        &Safeguards::default(),
        true,
    )
    .await
    .unwrap_or_else(|e| panic!("{label}: {e}\n{script}"));
    script
}

#[tokio::test]
async fn structure_forms_change_columns_on_every_engine() {
    for (label, session, schema) in engines().await {
        let cleanup = ["DROP TABLE oa_alter_t"];
        run(label, &session, &cleanup, true).await;
        let (wide, collate) = match label {
            "sqlserver" => ("VARCHAR(40)", " COLLATE Latin1_General_BIN"),
            "mysql" | "mariadb" => ("varchar(40)", " COLLATE utf8mb4_bin"),
            _ => ("VARCHAR(40)", ""),
        };
        run(
            label,
            &session,
            &[
                &format!(
                    "CREATE TABLE oa_alter_t (id INT NOT NULL PRIMARY KEY, qty INT, note VARCHAR(20){collate} DEFAULT 'x')"
                ),
                "INSERT INTO oa_alter_t (id, qty, note) VALUES (1, 5, 'a')",
                "INSERT INTO oa_alter_t (id, qty, note) VALUES (2, 7, 'b')",
            ],
            false,
        )
        .await;
        let before = structure(label, &session, schema, "oa_alter_t").await;
        let note = column(&before, "note");
        assert!(
            note.default.as_deref().is_some_and(|d| d.contains("'x'")),
            "{label}: {note:?}"
        );
        assert!(column(&before, "qty").nullable, "{label}");
        assert_eq!(before.comments, label != "sqlite", "{label}");
        assert_eq!(before.rebuilds, label == "sqlite", "{label}");
        assert_eq!(
            before.transactional,
            !matches!(label, "mysql" | "mariadb"),
            "{label}"
        );

        // Nothing typed differently: nothing to do.
        let unchanged = TableAlteration {
            schema: schema.into(),
            table: "oa_alter_t".into(),
            columns: before.columns.iter().map(edit_of).collect(),
            drop: Vec::new(),
        };
        assert!(
            matches!(
                alteration_plan(&session, &unchanged).await,
                Err(ObjectError::Unsupported(_))
            ),
            "{label}"
        );

        // Several columns at once: a type, the nullability and a default; a wider type that
        // keeps its default and collation; a comment.
        let mut qty = edit_of(column(&before, "qty"));
        qty.data_type = "BIGINT".into();
        qty.nullable = false;
        qty.default = Some("0".into());
        let mut note = edit_of(column(&before, "note"));
        note.data_type = wide.into();
        if before.comments {
            note.comment = Some("the customer's note".into());
        }
        let alteration = TableAlteration {
            columns: vec![qty, note],
            ..unchanged.clone()
        };
        let script = apply_form(label, &session, &alteration).await;
        let after = structure(label, &session, schema, "oa_alter_t").await;
        let qty = column(&after, "qty");
        assert!(
            qty.data_type.to_lowercase().contains("bigint"),
            "{label}: {qty:?}\n{script}"
        );
        assert!(!qty.nullable, "{label}: {qty:?}\n{script}");
        assert!(
            qty.default.as_deref().is_some_and(|d| d.contains('0')),
            "{label}: {qty:?}\n{script}"
        );
        let note = column(&after, "note");
        // DuckDB has no VARCHAR lengths.
        if label != "duckdb" {
            assert!(note.data_type.contains("40"), "{label}: {note:?}\n{script}");
        }
        assert!(
            note.default.as_deref().is_some_and(|d| d.contains("'x'")),
            "{label}: the default stays: {note:?}\n{script}"
        );
        if after.comments {
            assert_eq!(
                note.comment.as_deref(),
                Some("the customer's note"),
                "{label}\n{script}"
            );
        }
        // What the form did not touch stays as it was.
        match label {
            "sqlserver" => assert_eq!(
                text(
                    &session,
                    "SELECT collation_name FROM sys.columns WHERE object_id = OBJECT_ID('dbo.oa_alter_t') AND name = 'note'"
                )
                .await,
                "Latin1_General_BIN",
                "{script}"
            ),
            "mysql" | "mariadb" => assert_eq!(
                text(
                    &session,
                    "SELECT collation_name FROM information_schema.columns WHERE table_schema = 'spelaion' AND table_name = 'oa_alter_t' AND column_name = 'note'"
                )
                .await,
                "utf8mb4_bin",
                "{label}: {script}"
            ),
            _ => {}
        }
        // The rows are still there, and the new default and NOT NULL work.
        assert_eq!(
            text(&session, "SELECT note FROM oa_alter_t WHERE id = 2").await,
            "b",
            "{label}"
        );
        run(
            label,
            &session,
            &["INSERT INTO oa_alter_t (id) VALUES (3)"],
            false,
        )
        .await;
        assert_eq!(
            text(&session, "SELECT qty FROM oa_alter_t WHERE id = 3").await,
            "0",
            "{label}"
        );
        assert_eq!(
            text(&session, "SELECT note FROM oa_alter_t WHERE id = 3").await,
            "x",
            "{label}"
        );

        // Back again: nullable, no default, no comment.
        let mut qty = edit_of(column(&after, "qty"));
        qty.nullable = true;
        qty.default = None;
        let mut note = edit_of(column(&after, "note"));
        note.comment = None;
        let alteration = TableAlteration {
            columns: vec![qty, note],
            ..unchanged.clone()
        };
        let script = apply_form(label, &session, &alteration).await;
        let last = structure(label, &session, schema, "oa_alter_t").await;
        let qty = column(&last, "qty");
        assert!(
            qty.nullable && qty.default.is_none(),
            "{label}: {qty:?}\n{script}"
        );
        assert_eq!(column(&last, "note").comment, None, "{label}\n{script}");
        run(label, &session, &cleanup, true).await;
    }
}

#[tokio::test]
async fn mysql_restates_every_attribute_of_a_modified_column() {
    for (label, session, schema) in engines().await {
        if !matches!(label, "mysql" | "mariadb") {
            continue;
        }
        run(label, &session, &["DROP TABLE oa_modify_t"], true).await;
        run(
            label,
            &session,
            &["CREATE TABLE oa_modify_t (id INT AUTO_INCREMENT PRIMARY KEY, code VARCHAR(10) CHARACTER SET latin1 COLLATE latin1_bin NOT NULL DEFAULT 'it''s' COMMENT 'it''s kept', n INT)"],
            false,
        )
        .await;
        let before = structure(label, &session, schema, "oa_modify_t").await;
        let code = column(&before, "code").clone();
        assert!(
            code.default.as_deref().is_some_and(|d| d.starts_with('\'')),
            "{label}: the default is SQL: {code:?}"
        );
        let mut edit = edit_of(&code);
        edit.data_type = "varchar(30)".into();
        // A number keeps no character set: the type change drops it.
        let mut n = edit_of(column(&before, "n"));
        n.data_type = "bigint".into();
        n.comment = Some("a\\b".into());
        let script = apply_form(
            label,
            &session,
            &TableAlteration {
                schema: schema.into(),
                table: "oa_modify_t".into(),
                columns: vec![edit, n],
                drop: Vec::new(),
            },
        )
        .await;
        let definition = session
            .connection()
            .relation_definition(schema, "oa_modify_t")
            .await
            .unwrap();
        for kept in [
            "latin1_bin",
            "NOT NULL",
            "COMMENT 'it''s kept'",
            "AUTO_INCREMENT",
            "varchar(30)",
        ] {
            assert!(
                definition.contains(kept),
                "{label}: {kept}\n{script}\n{definition}"
            );
        }
        let after = structure(label, &session, schema, "oa_modify_t").await;
        assert_eq!(column(&after, "code").default, code.default, "{label}");
        assert_eq!(
            column(&after, "n").comment.as_deref(),
            Some("a\\b"),
            "{label}"
        );
        run(label, &session, &["DROP TABLE oa_modify_t"], true).await;
    }
}

#[tokio::test]
async fn constraints_drop_with_the_column_changes() {
    for (label, session, schema) in engines().await {
        if label == "duckdb" {
            continue;
        }
        let cleanup = ["DROP TABLE oa_cdrop_child", "DROP TABLE oa_cdrop_parent"];
        run(label, &session, &cleanup, true).await;
        run(
            label,
            &session,
            &[
                "CREATE TABLE oa_cdrop_parent (id INT NOT NULL PRIMARY KEY)",
                "CREATE TABLE oa_cdrop_child (id INT NOT NULL PRIMARY KEY, code VARCHAR(10), parent_id INT, CONSTRAINT oa_cdrop_code UNIQUE (code), CONSTRAINT oa_cdrop_fk FOREIGN KEY (parent_id) REFERENCES oa_cdrop_parent (id))",
                "INSERT INTO oa_cdrop_parent (id) VALUES (1)",
                "INSERT INTO oa_cdrop_child (id, code, parent_id) VALUES (1, 'a', 1)",
            ],
            false,
        )
        .await;
        let info = session
            .connection()
            .describe_table(schema, "oa_cdrop_child")
            .await
            .unwrap();
        let unique = info
            .constraints
            .iter()
            .find(|c| c.kind == ConstraintKind::Unique)
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| panic!("{label}: {info:?}"));
        let fk = info.foreign_keys[0].name.clone();
        let before = structure(label, &session, schema, "oa_cdrop_child").await;
        let mut code = edit_of(column(&before, "code"));
        code.default = Some("'z'".into());
        let script = apply_form(
            label,
            &session,
            &TableAlteration {
                schema: schema.into(),
                table: "oa_cdrop_child".into(),
                columns: vec![code],
                drop: vec![
                    DroppedConstraint::Constraint { name: unique },
                    DroppedConstraint::ForeignKey { name: fk },
                ],
            },
        )
        .await;
        let info = session
            .connection()
            .describe_table(schema, "oa_cdrop_child")
            .await
            .unwrap();
        assert!(info.foreign_keys.is_empty(), "{label}: {info:?}\n{script}");
        assert!(
            !info
                .constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Unique),
            "{label}: {info:?}\n{script}"
        );
        // Without them, a duplicate code and a parent that does not exist are accepted.
        run(
            label,
            &session,
            &["INSERT INTO oa_cdrop_child (id, code, parent_id) VALUES (2, 'a', 99)"],
            false,
        )
        .await;
        assert_eq!(
            text(&session, "SELECT code FROM oa_cdrop_child WHERE id = 1").await,
            "a",
            "{label}"
        );
        run(label, &session, &cleanup, true).await;
    }
}

async fn sqlite_memory() -> Session {
    session(
        &spelaion_driver_sqlite::SqliteDriver::default(),
        ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        },
    )
    .await
}

#[tokio::test]
async fn sqlite_rebuild_keeps_rows_indexes_triggers_views_and_keys() {
    let session = sqlite_memory().await;
    run(
        "sqlite",
        &session,
        &[
            "PRAGMA foreign_keys = ON",
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE TABLE item (id INTEGER PRIMARY KEY AUTOINCREMENT, parent_id INTEGER REFERENCES parent (id), qty TEXT NOT NULL, code TEXT UNIQUE COLLATE NOCASE, total INTEGER GENERATED ALWAYS AS (qty * 2) VIRTUAL, CHECK (qty <> ''))",
            "CREATE INDEX item_qty ON item (qty)",
            "CREATE TABLE audit (n INTEGER)",
            "CREATE TRIGGER item_added AFTER INSERT ON item BEGIN INSERT INTO audit VALUES (new.id); END",
            "CREATE VIEW item_view AS SELECT id, qty FROM item",
            "CREATE VIEW item_view_2 AS SELECT * FROM item_view",
            "CREATE TABLE other (x INTEGER)",
            "CREATE TRIGGER other_added AFTER INSERT ON other BEGIN DELETE FROM item WHERE id < 0; END",
            "CREATE TABLE child (item_id INTEGER REFERENCES item (id))",
            "INSERT INTO parent VALUES (1, 'p')",
            "INSERT INTO item (parent_id, qty, code) VALUES (1, '10', 'a')",
            "INSERT INTO item (parent_id, qty, code) VALUES (1, '20', 'b')",
            "INSERT INTO item (parent_id, qty, code) VALUES (1, '30', 'c')",
            "DELETE FROM item WHERE id = 3",
            "INSERT INTO child VALUES (2)",
        ],
        false,
    )
    .await;
    let before = structure("sqlite", &session, "main", "item").await;
    assert_eq!(column(&before, "qty").data_type, "TEXT");
    assert!(column(&before, "total").generated);
    let mut qty = edit_of(column(&before, "qty"));
    qty.data_type = "INTEGER".into();
    qty.default = Some("1".into());
    let mut code = edit_of(column(&before, "code"));
    code.nullable = false;
    let alteration = TableAlteration {
        schema: "main".into(),
        table: "item".into(),
        columns: vec![qty, code],
        drop: Vec::new(),
    };
    let plan = alteration_plan(&session, &alteration).await.unwrap();
    assert!(plan.rebuilds() && plan.transactional());
    let script = plan.script();
    // The rebuild drops a table: it needs the user's confirmation on every connection.
    assert!(
        !plan
            .risks(session.dialect().as_ref(), &Safeguards::default())
            .is_empty()
    );
    assert_eq!(
        apply_alteration(
            &session,
            &alteration,
            &script,
            false,
            &Safeguards::default(),
            false
        )
        .await,
        Err(ObjectError::NeedsConfirmation)
    );
    assert_eq!(
        apply_alteration(
            &session,
            &alteration,
            &script,
            true,
            &Safeguards::default(),
            true
        )
        .await,
        Err(ObjectError::ReadOnly)
    );
    apply_alteration(
        &session,
        &alteration,
        &script,
        false,
        &Safeguards::default(),
        true,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}\n{script}"));

    let after = structure("sqlite", &session, "main", "item").await;
    assert_eq!(column(&after, "qty").data_type, "INTEGER", "{script}");
    assert_eq!(column(&after, "qty").default.as_deref(), Some("1"));
    assert!(!column(&after, "code").nullable);
    // Rows, converted to the new affinity, with their generated column.
    assert_eq!(
        text(&session, "SELECT typeof(qty) FROM item WHERE id = 2").await,
        "integer"
    );
    assert_eq!(
        text(&session, "SELECT total FROM item WHERE id = 2").await,
        "40"
    );
    assert_eq!(text(&session, "SELECT count(*) FROM item").await, "2");
    // The index, the unique constraint, its collation and the check are still there.
    let definition = session
        .connection()
        .relation_definition("main", "item")
        .await
        .unwrap();
    for kept in [
        "CREATE INDEX item_qty ON item (qty)",
        "AUTOINCREMENT",
        "UNIQUE COLLATE NOCASE",
        "CHECK (qty <> '')",
        "REFERENCES parent (id)",
    ] {
        assert!(definition.contains(kept), "{kept}\n{definition}");
    }
    // The views and triggers work, and AUTOINCREMENT goes on from its old counter.
    run(
        "sqlite",
        &session,
        &["INSERT INTO item (parent_id, qty, code) VALUES (1, 5, 'd')"],
        false,
    )
    .await;
    assert_eq!(text(&session, "SELECT max(id) FROM item").await, "4");
    assert_eq!(text(&session, "SELECT max(n) FROM audit").await, "4");
    assert_eq!(
        text(&session, "SELECT count(*) FROM item_view_2").await,
        "3"
    );
    run("sqlite", &session, &["INSERT INTO other VALUES (1)"], false).await;
    assert_eq!(text(&session, "PRAGMA foreign_keys").await, "1");
    // The child's key still points to the table.
    assert_eq!(
        text(
            &session,
            "SELECT count(*) FROM pragma_foreign_key_check('child')"
        )
        .await,
        "0"
    );
    assert!(
        session
            .connection()
            .relation_definition("main", "child")
            .await
            .unwrap()
            .contains("REFERENCES item (id)")
    );
    // A unique constraint and a foreign key dropped by a rebuild, from the columns' clauses.
    let info = session
        .connection()
        .describe_table("main", "item")
        .await
        .unwrap();
    let unique = info
        .constraints
        .iter()
        .find(|c| c.kind == ConstraintKind::Unique)
        .and_then(|c| c.name.clone())
        .unwrap();
    let fk = info.foreign_keys[0].name.clone();
    apply_form(
        "sqlite",
        &session,
        &TableAlteration {
            drop: vec![
                DroppedConstraint::Constraint { name: unique },
                DroppedConstraint::ForeignKey { name: fk },
            ],
            columns: Vec::new(),
            ..alteration.clone()
        },
    )
    .await;
    let definition = session
        .connection()
        .relation_definition("main", "item")
        .await
        .unwrap();
    assert!(
        !definition.contains("UNIQUE") && !definition.contains("REFERENCES"),
        "{definition}"
    );
    assert!(definition.contains("COLLATE NOCASE"), "{definition}");
    assert!(definition.contains("CREATE INDEX item_qty"), "{definition}");
    assert_eq!(text(&session, "SELECT count(*) FROM item").await, "3");
}

#[tokio::test]
async fn a_sqlite_rebuild_that_breaks_a_key_changes_nothing() {
    let session = sqlite_memory().await;
    run(
        "sqlite",
        &session,
        &[
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE item (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent (id), qty TEXT)",
            // Written with the keys off: the table already breaks its key.
            "PRAGMA foreign_keys = OFF",
            "INSERT INTO item VALUES (1, 99, 'a')",
            "PRAGMA foreign_keys = ON",
        ],
        false,
    )
    .await;
    let before = structure("sqlite", &session, "main", "item").await;
    let mut qty = edit_of(column(&before, "qty"));
    qty.data_type = "BLOB".into();
    let alteration = TableAlteration {
        schema: "main".into(),
        table: "item".into(),
        columns: vec![qty],
        drop: Vec::new(),
    };
    let script = alteration_plan(&session, &alteration)
        .await
        .unwrap()
        .script();
    match apply_alteration(
        &session,
        &alteration,
        &script,
        false,
        &Safeguards::default(),
        true,
    )
    .await
    {
        Err(ObjectError::Failed { message, .. }) => {
            assert!(message.contains("foreign keys"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        column(&structure("sqlite", &session, "main", "item").await, "qty").data_type,
        "TEXT"
    );
    assert_eq!(text(&session, "PRAGMA foreign_keys").await, "1");
    assert_eq!(relations(&session, "main").await, ["item", "parent"]);
    // Nothing is left half done: no transaction stays open.
    run("sqlite", &session, &["BEGIN", "COMMIT"], false).await;
}

#[tokio::test]
async fn a_form_runs_only_the_script_it_showed() {
    let session = sqlite_memory().await;
    run(
        "sqlite",
        &session,
        &["CREATE TABLE t (a TEXT, b TEXT)"],
        false,
    )
    .await;
    let before = structure("sqlite", &session, "main", "t").await;
    let mut a = edit_of(column(&before, "a"));
    a.default = Some("'x'".into());
    let alteration = TableAlteration {
        schema: "main".into(),
        table: "t".into(),
        columns: vec![a],
        drop: Vec::new(),
    };
    let script = alteration_plan(&session, &alteration)
        .await
        .unwrap()
        .script();
    // Someone changes the table after the preview.
    run(
        "sqlite",
        &session,
        &["ALTER TABLE t ADD COLUMN c TEXT"],
        false,
    )
    .await;
    assert_eq!(
        apply_alteration(
            &session,
            &alteration,
            &script,
            false,
            &Safeguards::default(),
            true
        )
        .await,
        Err(ObjectError::Changed)
    );
    // A production type asks for confirmation of any change.
    let production = Safeguards {
        confirm_data_changes: true,
        ..Safeguards::default()
    };
    let plan = alteration_plan(&session, &alteration).await.unwrap();
    assert!(
        !plan
            .risks(session.dialect().as_ref(), &production)
            .is_empty()
    );
}

fn new_column(name: &str, ty: &str, nullable: bool) -> NewColumn {
    NewColumn {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        default: None,
        comment: None,
    }
}

#[tokio::test]
async fn the_create_table_form_runs_on_every_engine() {
    for (label, session, schema) in engines().await {
        let cleanup = ["DROP TABLE oa_new_line", "DROP TABLE oa_new_head"];
        run(label, &session, &cleanup, true).await;
        run(
            label,
            &session,
            &["CREATE TABLE oa_new_head (id INT NOT NULL PRIMARY KEY)"],
            false,
        )
        .await;
        let comments = label != "sqlite";
        let mut note = new_column("note", "VARCHAR(40)", true);
        note.default = Some("'none'".into());
        if comments {
            note.comment = Some("what it's for".into());
        }
        let table = NewTable {
            schema: schema.into(),
            name: "oa_new_line".into(),
            columns: vec![
                new_column("head_id", "INT", false),
                new_column("line", "INT", false),
                note,
            ],
            primary_key: vec!["head_id".into(), "line".into()],
            foreign_keys: vec![NewForeignKey {
                name: String::new(),
                columns: vec!["head_id".into()],
                referenced_schema: schema.into(),
                referenced_table: "oa_new_head".into(),
                referenced_columns: vec!["id".into()],
            }],
            indexes: vec![NewIndex {
                name: String::new(),
                columns: vec!["note".into()],
                unique: false,
            }],
        };
        let plan = create_table_plan(session.dialect().as_ref(), &table)
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let script = plan.script();
        apply_create_table(
            &session,
            &table,
            &script,
            false,
            &Safeguards::default(),
            false,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}\n{script}"));
        let info = session
            .connection()
            .describe_table(schema, "oa_new_line")
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}\n{script}"));
        assert_eq!(
            info.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["head_id", "line", "note"],
            "{label}"
        );
        assert_eq!(info.primary_key, ["head_id", "line"], "{label}\n{script}");
        assert_eq!(info.foreign_keys.len(), 1, "{label}\n{script}");
        assert_eq!(
            info.foreign_keys[0].referenced_table, "oa_new_head",
            "{label}"
        );
        assert!(
            info.indexes
                .iter()
                .any(|i| i.name == "oa_new_line_note_idx"),
            "{label}: {:?}\n{script}",
            info.indexes
        );
        let note = info.columns.iter().find(|c| c.name == "note").unwrap();
        assert!(
            note.default.as_deref().is_some_and(|d| d.contains("none")),
            "{label}: {note:?}"
        );
        if comments {
            assert_eq!(note.comment.as_deref(), Some("what it's for"), "{label}");
        }
        // Only the script it showed: a different one is refused, and so is a read-only
        // connection.
        assert_eq!(
            apply_create_table(
                &session,
                &table,
                "CREATE TABLE other (x INT);\n",
                false,
                &Safeguards::default(),
                true
            )
            .await,
            Err(ObjectError::Changed),
            "{label}"
        );
        assert_eq!(
            apply_create_table(
                &session,
                &table,
                &script,
                true,
                &Safeguards::default(),
                true
            )
            .await,
            Err(ObjectError::ReadOnly),
            "{label}"
        );
        run(label, &session, &cleanup, true).await;
    }
}

async fn listed_object(session: &Session, schema: &str, name: &str) -> SchemaObject {
    session
        .connection()
        .list_schema_objects(schema)
        .await
        .unwrap()
        .into_iter()
        .find(|o| o.name == name)
        .unwrap_or_else(|| panic!("{name} is not listed"))
}

#[tokio::test]
async fn routines_are_called_with_their_parameters_on_every_engine() {
    for (label, session, schema) in engines().await {
        let (create, cleanup): (&str, &str) = match label {
            "postgres" => (
                "CREATE FUNCTION oa_add(a integer, b integer) RETURNS integer LANGUAGE sql AS 'SELECT a + b'",
                "DROP FUNCTION oa_add",
            ),
            "mysql" | "mariadb" => (
                "CREATE FUNCTION oa_add(a INT, b INT) RETURNS INT DETERMINISTIC RETURN a + b",
                "DROP FUNCTION oa_add",
            ),
            "sqlserver" => (
                "CREATE FUNCTION dbo.oa_add(@a INT, @b INT) RETURNS INT AS BEGIN RETURN @a + @b END",
                "DROP FUNCTION dbo.oa_add",
            ),
            "duckdb" => ("CREATE MACRO oa_add(a, b) AS a + b", "DROP MACRO oa_add"),
            // SQLite has no functions of its own.
            _ => continue,
        };
        run(label, &session, &[cleanup], true).await;
        run(label, &session, &[create], false).await;
        let object = listed_object(&session, schema, "oa_add").await;
        let call = session
            .connection()
            .procedure_call(&object)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let dialect = session.dialect();
        assert_eq!(
            spelaion_core::script_params(dialect.as_ref(), &call),
            ["a", "b"],
            "{label}: {call}"
        );
        let values = [
            ("a".to_string(), "2".to_string()),
            ("b".to_string(), "3".to_string()),
        ]
        .into_iter()
        .collect();
        let bound = spelaion_core::bind_params(dialect.as_ref(), &call, &values).unwrap();
        assert_eq!(text(&session, &bound).await, "5", "{label}: {bound}");
        run(label, &session, &[cleanup], true).await;
    }
}

/// Changes `41` to `42` in an editable source and runs it as the editor would, as one script.
async fn edit_and_run(label: &str, session: &Session, target: SourceTarget) -> String {
    let source = editable_source(session, &target)
        .await
        .unwrap_or_else(|e| panic!("{label}: {target:?}: {e}"));
    assert!(source.contains("41"), "{label}: {source}");
    let edited = source.replacen("41", "42", 1);
    run(label, session, &[edited.as_str()], false).await;
    edited
}

#[tokio::test]
async fn edited_sources_replace_views_and_routines_on_every_engine() {
    for (label, session, schema) in engines().await {
        let (function, procedure, cleanup): (Option<&str>, Option<&str>, &[&str]) = match label {
            "postgres" => (
                Some("CREATE FUNCTION oa_src_f() RETURNS integer LANGUAGE sql AS 'SELECT 41'"),
                Some("CREATE PROCEDURE oa_src_p() LANGUAGE sql AS 'SELECT 41'"),
                &[
                    "DROP VIEW oa_src_v",
                    "DROP FUNCTION oa_src_f",
                    "DROP PROCEDURE oa_src_p",
                ],
            ),
            "mysql" | "mariadb" => (
                Some("CREATE FUNCTION oa_src_f() RETURNS INT DETERMINISTIC RETURN 41"),
                Some(
                    "DELIMITER $$
CREATE PROCEDURE oa_src_p() BEGIN SELECT 41; SELECT 1; END$$
DELIMITER ;",
                ),
                &[
                    "DROP VIEW oa_src_v",
                    "DROP FUNCTION oa_src_f",
                    "DROP PROCEDURE oa_src_p",
                ],
            ),
            "sqlserver" => (
                Some("CREATE FUNCTION dbo.oa_src_f() RETURNS INT AS BEGIN RETURN 41 END"),
                Some("CREATE PROCEDURE dbo.oa_src_p AS SELECT 41"),
                &[
                    "DROP VIEW oa_src_v",
                    "DROP FUNCTION oa_src_f",
                    "DROP PROCEDURE oa_src_p",
                ],
            ),
            "duckdb" => (
                Some("CREATE MACRO oa_src_f() AS 41"),
                None,
                &["DROP VIEW oa_src_v", "DROP MACRO oa_src_f"],
            ),
            _ => (None, None, &["DROP VIEW oa_src_v"]),
        };
        run(label, &session, cleanup, true).await;
        run(
            label,
            &session,
            &["CREATE VIEW oa_src_v AS SELECT 41 AS n"],
            false,
        )
        .await;
        let edited = edit_and_run(
            label,
            &session,
            SourceTarget::View {
                schema: schema.into(),
                name: "oa_src_v".into(),
            },
        )
        .await;
        assert_eq!(
            text(&session, "SELECT n FROM oa_src_v").await,
            "42",
            "{label}: {edited}"
        );
        if let Some(function) = function {
            run(label, &session, &[function], false).await;
            let object = listed_object(&session, schema, "oa_src_f").await;
            let edited = edit_and_run(label, &session, SourceTarget::Routine(object.clone())).await;
            let call = session.connection().procedure_call(&object).await.unwrap();
            assert_eq!(text(&session, &call).await, "42", "{label}: {edited}");
        }
        if let Some(procedure) = procedure {
            run(label, &session, &[procedure], false).await;
            let object = listed_object(&session, schema, "oa_src_p").await;
            let edited = edit_and_run(label, &session, SourceTarget::Routine(object.clone())).await;
            let definition = session
                .connection()
                .object_definition(&object)
                .await
                .unwrap();
            assert!(
                definition.contains("42") && !definition.contains("41"),
                "{label}: {edited}\n{definition}"
            );
        }
        // A table has no source to edit here.
        assert!(
            editable_source(
                &session,
                &SourceTarget::Routine(SchemaObject {
                    schema: schema.into(),
                    name: "oa_src_v".into(),
                    kind: SchemaObjectKind::Sequence,
                    detail: None,
                    comment: None,
                })
            )
            .await
            .is_err(),
            "{label}"
        );
        run(label, &session, cleanup, true).await;
    }
}
