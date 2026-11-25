//! Schema compare (ADR-070) against every engine it serves: SQLite and DuckDB always,
//! PostgreSQL, MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without
//! them they are skipped, which does not count as tested).
//!
//! - Two schemas with known differences of every kind report exactly those: tables, columns
//!   (type, nullability, default), indexes, views and functions only on one side or changed,
//!   and a table that is the same counted, not listed.
//! - A schema compared with itself, or with an equal copy, reports nothing.
//! - MySQL is not compared with MariaDB, and a cancel before the first read reads nothing.

use crate::engines;

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    CompareError, CompareObjectKind, ComparedPart, CoreEvent, JobCancel, MigrationOptions,
    ObjectStatus, PartStatus, Property, ResultStore, SchemaComparison, ServerFamily, Session,
    compare_schemas, compare_support, count_table_rows, migration_script,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

const SOURCE: &str = "cmp_src";
const TARGET: &str = "cmp_tgt";
const COPY: &str = "cmp_copy";

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

/// Every engine available here. MySQL and MariaDB connect as root, which may create databases
/// (their schemas) and functions. The server family is what `open_connection` reads from the
/// server; these sessions are opened without it.
async fn engines() -> Vec<(&'static str, Session)> {
    let memory = || ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let mut out = vec![
        ("sqlite", session(&*engines::sqlite(), memory()).await),
        ("duckdb", session(&*engines::duckdb(), memory()).await),
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
            session(&*engines::postgres(), params)
                .await
                .with_server_family(ServerFamily::PostgreSql),
        ));
    }
    for (label, var, driver, family) in [
        (
            "mysql",
            "SPELAION_TEST_MYSQL",
            engines::mysql(),
            ServerFamily::MySql,
        ),
        (
            "mariadb",
            "SPELAION_TEST_MARIADB",
            engines::mariadb(),
            ServerFamily::MariaDb,
        ),
    ] {
        if let Some((host, port)) = server(var) {
            let params = login(host, port, "root", "spelaion-root", "spelaion");
            out.push((
                label,
                session(&driver, params).await.with_server_family(family),
            ));
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push(("sqlserver", session(&*engines::sqlserver(), params).await));
    }
    out
}

/// Runs each statement on its own; panics on a failure unless `tolerate`.
async fn run(label: &str, session: &Session, statements: &[String], tolerate: bool) {
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            if let CoreEvent::StatementFailed { error, .. } = event
                && !tolerate
            {
                panic!(
                    "{label}: {statement}
→ {}",
                    error.message
                );
            }
        }
    }
}

/// The statements that create an empty schema, and those that remove it with what it holds.
fn schema_statements(label: &str, schema: &str) -> (Vec<String>, Vec<String>) {
    match label {
        "sqlite" => (
            vec![format!("ATTACH DATABASE ':memory:' AS {schema}")],
            vec![format!("DETACH DATABASE {schema}")],
        ),
        "mysql" | "mariadb" => (
            vec![format!("CREATE DATABASE {schema}")],
            vec![format!("DROP DATABASE IF EXISTS {schema}")],
        ),
        "sqlserver" => (
            vec![format!("CREATE SCHEMA {schema}")],
            vec![
                format!("DROP VIEW IF EXISTS {schema}.v_items"),
                format!("DROP VIEW IF EXISTS {schema}.only_v"),
                format!("DROP FUNCTION IF EXISTS {schema}.f"),
                format!("DROP TABLE IF EXISTS {schema}.items"),
                format!("DROP TABLE IF EXISTS {schema}.same_t"),
                format!("DROP TABLE IF EXISTS {schema}.only_s"),
                format!("DROP TABLE IF EXISTS {schema}.only_t"),
                format!("DROP SCHEMA IF EXISTS {schema}"),
            ],
        ),
        // PostgreSQL and DuckDB.
        _ => (
            vec![format!("CREATE SCHEMA {schema}")],
            vec![format!("DROP SCHEMA IF EXISTS {schema} CASCADE")],
        ),
    }
}

fn index(label: &str, schema: &str, name: &str, table: &str, column: &str) -> String {
    match label {
        // SQLite puts the schema on the index, and the table is its own schema's.
        "sqlite" => format!("CREATE INDEX {schema}.{name} ON {table} ({column})"),
        _ => format!("CREATE INDEX {name} ON {schema}.{table} ({column})"),
    }
}

fn view(label: &str, schema: &str, name: &str, select: &str, table: &str) -> String {
    match label {
        // A SQLite view names its tables without the schema: they are its own schema's.
        "sqlite" => format!("CREATE VIEW {schema}.{name} AS SELECT {select} FROM {table}"),
        _ => format!("CREATE VIEW {schema}.{name} AS SELECT {select} FROM {schema}.{table}"),
    }
}

/// A function returning `body` (an expression of `x`), where the engine has functions.
fn function(label: &str, schema: &str, body: &str) -> Option<String> {
    match label {
        "postgres" => Some(format!(
            "CREATE FUNCTION {schema}.f(x integer) RETURNS integer LANGUAGE sql AS 'SELECT {body}'"
        )),
        "mysql" | "mariadb" => Some(format!(
            "CREATE FUNCTION {schema}.f(x INT) RETURNS INT DETERMINISTIC RETURN {body}"
        )),
        "sqlserver" => Some(format!(
            "CREATE FUNCTION {schema}.f(@x INT) RETURNS INT AS BEGIN RETURN {} END",
            body.replace('x', "@x")
        )),
        "duckdb" => Some(format!("CREATE MACRO {schema}.f(x) AS {body}")),
        _ => None,
    }
}

/// The source schema: what the target should become.
fn source_statements(label: &str, s: &str) -> Vec<String> {
    let mut out = vec![
        format!(
            "CREATE TABLE {s}.items (id INTEGER NOT NULL PRIMARY KEY, name VARCHAR(40) NOT NULL, \
             price DECIMAL(10,2), qty INTEGER DEFAULT 1, note VARCHAR(20))"
        ),
        format!("CREATE TABLE {s}.same_t (id INTEGER NOT NULL PRIMARY KEY, label VARCHAR(10))"),
        format!("CREATE TABLE {s}.only_s (id INTEGER NOT NULL PRIMARY KEY)"),
        index(label, s, "items_name_idx", "items", "name"),
        view(label, s, "v_items", "id, name", "items"),
        view(label, s, "only_v", "id", "same_t"),
    ];
    out.extend(function(label, s, "x + 1"));
    out
}

/// The target schema, with a difference of every kind from the source.
fn target_statements(label: &str, t: &str) -> Vec<String> {
    let mut out = vec![
        format!(
            "CREATE TABLE {t}.items (id INTEGER NOT NULL PRIMARY KEY, name VARCHAR(40), \
             price DECIMAL(12,2), qty INTEGER DEFAULT 2, legacy VARCHAR(20))"
        ),
        format!("CREATE TABLE {t}.same_t (id INTEGER NOT NULL PRIMARY KEY, label VARCHAR(10))"),
        format!("CREATE TABLE {t}.only_t (id INTEGER NOT NULL PRIMARY KEY)"),
        view(label, t, "v_items", "id", "items"),
    ];
    out.extend(function(label, t, "x"));
    out
}

async fn compared(label: &str, session: &Session, source: &str, target: &str) -> SchemaComparison {
    compare_schemas(session, source, session, target, &JobCancel::new(), &|_| {})
        .await
        .unwrap_or_else(|e| panic!("{label}: comparing {source} with {target}: {e}"))
        .report
}

fn status_of<'a>(
    comparison: &'a SchemaComparison,
    kind: CompareObjectKind,
    name: &str,
) -> Option<&'a ObjectStatus> {
    comparison
        .objects
        .iter()
        .find(|o| o.kind == kind && o.name == name)
        .map(|o| &o.status)
}

fn part<'a>(status: &'a ObjectStatus, part: &ComparedPart) -> Option<&'a PartStatus> {
    match status {
        ObjectStatus::Changed(changes) => changes
            .parts
            .iter()
            .find(|p| &p.part == part)
            .map(|p| &p.status),
        ObjectStatus::OnlyInSource | ObjectStatus::OnlyInTarget => None,
    }
}

fn changed_properties(status: Option<&PartStatus>) -> Vec<Property> {
    match status {
        Some(PartStatus::Changed(changes)) => changes.iter().map(|c| c.property).collect(),
        Some(PartStatus::OnlyInSource(_) | PartStatus::OnlyInTarget(_)) | None => Vec::new(),
    }
}

#[tokio::test]
async fn known_differences_are_reported_on_every_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    for (label, session) in engines().await {
        assert_eq!(compare_support(&session), Ok(()), "{label}");
        let mut cleanup = Vec::new();
        for schema in [SOURCE, TARGET, COPY] {
            let (create, drop) = schema_statements(label, schema);
            run(label, &session, &drop, true).await;
            run(label, &session, &create, false).await;
            cleanup.extend(drop);
        }
        run(label, &session, &source_statements(label, SOURCE), false).await;
        run(label, &session, &target_statements(label, TARGET), false).await;
        run(label, &session, &source_statements(label, COPY), false).await;

        let comparison = compared(label, &session, SOURCE, TARGET).await;
        let table = |name| status_of(&comparison, CompareObjectKind::Table, name);
        assert_eq!(
            table("only_s"),
            Some(&ObjectStatus::OnlyInSource),
            "{label}"
        );
        assert_eq!(
            table("only_t"),
            Some(&ObjectStatus::OnlyInTarget),
            "{label}"
        );
        assert_eq!(table("same_t"), None, "{label}: same_t is the same");
        let items = table("items").unwrap_or_else(|| panic!("{label}: items differs"));
        let column = |name: &str| part(items, &ComparedPart::Column(name.into()));
        assert_eq!(
            changed_properties(column("name")),
            [Property::Nullable],
            "{label}: {items:?}"
        );
        assert_eq!(
            changed_properties(column("price")),
            [Property::Type],
            "{label}"
        );
        assert_eq!(
            changed_properties(column("qty")),
            [Property::Default],
            "{label}"
        );
        assert!(
            matches!(column("note"), Some(PartStatus::OnlyInSource(_))),
            "{label}"
        );
        assert!(
            matches!(column("legacy"), Some(PartStatus::OnlyInTarget(_))),
            "{label}"
        );
        assert!(
            matches!(
                part(items, &ComparedPart::Index("items_name_idx".into())),
                Some(PartStatus::OnlyInSource(_))
            ),
            "{label}: {items:?}"
        );
        assert!(
            part(items, &ComparedPart::PrimaryKey).is_none(),
            "{label}: the key is the same"
        );
        assert!(
            matches!(
                status_of(&comparison, CompareObjectKind::View, "v_items"),
                Some(ObjectStatus::Changed(changes)) if !changes.source_diff.is_empty()
            ),
            "{label}: {comparison:?}"
        );
        assert_eq!(
            status_of(&comparison, CompareObjectKind::View, "only_v"),
            Some(&ObjectStatus::OnlyInSource),
            "{label}"
        );
        if function(label, SOURCE, "x").is_some() {
            assert!(
                matches!(
                    status_of(&comparison, CompareObjectKind::Function, "f"),
                    Some(ObjectStatus::Changed(_))
                ),
                "{label}: {comparison:?}"
            );
        }
        assert!(comparison.same >= 1, "{label}");

        // Equal schemas, and a schema with itself, report nothing.
        for (a, b) in [(SOURCE, COPY), (SOURCE, SOURCE)] {
            let comparison = compared(label, &session, a, b).await;
            assert!(
                comparison.is_empty(),
                "{label}: {a} against {b}: {:?}",
                comparison.objects
            );
        }
        run(label, &session, &cleanup, true).await;
    }
}

/// Builds the source and the target with rows in the target, writes the script with or
/// without drops, runs it on the target, and compares again. Each test passes schema names of
/// its own: the tests of this file run in parallel on the same servers.
async fn round_trip(
    label: &str,
    session: &Session,
    (source, target): (&str, &str),
    drops: bool,
) -> SchemaComparison {
    let mut cleanup = Vec::new();
    for schema in [source, target] {
        let (create, drop) = schema_statements(label, schema);
        run(label, session, &drop, true).await;
        run(label, session, &create, false).await;
        cleanup.extend(drop);
    }
    run(label, session, &source_statements(label, source), false).await;
    run(label, session, &target_statements(label, target), false).await;
    run(
        label,
        session,
        &[
            format!(
                "INSERT INTO {target}.items (id, name, price, qty, legacy) VALUES (1, 'a', 1.50, 2, 'x')"
            ),
            format!("INSERT INTO {target}.items (id, name, price, qty, legacy) VALUES (2, 'b', 2.25, 3, 'y')"),
            format!("INSERT INTO {target}.same_t (id, label) VALUES (1, 'kept')"),
        ],
        false,
    )
    .await;
    let first = compare_schemas(session, source, session, target, &JobCancel::new(), &|_| {})
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
    let script = migration_script(session, &first, &MigrationOptions { drops })
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
    assert!(
        script.not_scripted.is_empty(),
        "{label}: {:?}\n{}",
        script.not_scripted,
        script.sql
    );
    let events: Vec<CoreEvent> = session.run_script(&script.sql).collect().await;
    for event in &events {
        if let CoreEvent::StatementFailed { error, .. } = event {
            panic!(
                "{label}: the script failed: {}\n{}",
                error.message, script.sql
            );
        }
    }
    let again = compared(label, session, source, target).await;
    for (table, rows) in [("items", 2), ("same_t", 1)] {
        assert_eq!(
            count_table_rows(session, target, table, "").await,
            Ok(rows),
            "{label}: the rows of {table} stay"
        );
    }
    run(label, session, &cleanup, true).await;
    again
}

#[tokio::test]
async fn without_drops_the_script_leaves_only_what_is_only_in_the_target() {
    let _turn = crate::serial::file_turn(module_path!());
    for (label, session) in engines().await {
        let again = round_trip(label, &session, ("cmp_keep_src", "cmp_keep_tgt"), false).await;
        for object in &again.objects {
            let only_target = match &object.status {
                ObjectStatus::OnlyInTarget => true,
                ObjectStatus::Changed(changes) => changes
                    .parts
                    .iter()
                    .all(|p| matches!(p.status, PartStatus::OnlyInTarget(_))),
                ObjectStatus::OnlyInSource => false,
            };
            assert!(only_target, "{label}: {object:?}");
        }
        assert_eq!(
            status_of(&again, CompareObjectKind::Table, "only_t"),
            Some(&ObjectStatus::OnlyInTarget),
            "{label}"
        );
    }
}

#[tokio::test]
async fn with_drops_a_second_compare_is_empty_and_the_target_keeps_its_rows() {
    let _turn = crate::serial::file_turn(module_path!());
    for (label, session) in engines().await {
        let again = round_trip(label, &session, ("cmp_drop_src", "cmp_drop_tgt"), true).await;
        assert!(again.is_empty(), "{label}: {:?}", again.objects);
    }
}

#[tokio::test]
async fn mysql_is_not_compared_with_mariadb_and_a_cancel_reads_nothing() {
    let _turn = crate::serial::file_turn(module_path!());
    let sessions = engines().await;
    let find = |label| sessions.iter().find(|(l, _)| *l == label).map(|(_, s)| s);
    if let (Some(mysql), Some(mariadb)) = (find("mysql"), find("mariadb")) {
        let refused = compare_schemas(
            mysql,
            "spelaion",
            mariadb,
            "spelaion",
            &JobCancel::new(),
            &|_| {},
        )
        .await;
        assert!(
            matches!(&refused, Err(CompareError::Unsupported(reason)) if reason.contains("same engine")),
            "{refused:?}"
        );
    }
    let (_, sqlite) = &sessions[0];
    let cancel = JobCancel::new();
    cancel.cancel().await;
    let cancelled = compare_schemas(sqlite, "main", sqlite, "main", &cancel, &|_| {}).await;
    assert_eq!(cancelled, Err(CompareError::Cancelled));
}
