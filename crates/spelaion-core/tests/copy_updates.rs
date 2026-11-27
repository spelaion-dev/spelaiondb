//! Copying rows as UPDATE statements (SDB-388) on every engine whose edits the app applies:
//! SQLite and DuckDB always; PostgreSQL, MySQL, MariaDB and SQL Server with their
//! `SPELAION_TEST_*` variables (without them they are skipped, which does not count as
//! tested). ClickHouse, with `SPELAION_TEST_CLICKHOUSE`, is refused: it declares no
//! transactions, so the app applies no edits there.
//!
//! On each engine a table with a composite primary key holds values that are easy to write
//! wrongly (a quote, a backslash, a line break, accents, a negative decimal, NULL, bytes, a
//! date and a timestamp). Its rows are copied as UPDATE statements, from a query's result and
//! from a table data tab, the rows are changed, and the copied statements, run, must bring
//! every cell back. A join, a result read through another session, a masked column, a
//! selection of key columns only and a table without a primary key are refused with their
//! reason.

use crate::engines;

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::MaskingRules;
use spelaion_core::{
    CopySpecial, CoreEvent, MASK, MaskPolicy, ResultId, ResultStore, Session, UpdateCopyError,
    UpdateSource, copy_special, copy_updates, update_copy_table,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

const TABLE: &str = "copy_updates";
const QUERY: &str = "select * from copy_updates order by a, b";

/// One engine: how to reach it and how its table is written.
struct Case {
    label: &'static str,
    driver: Arc<dyn Driver>,
    params: ConnectParams,
    /// The schema the table is in, as a table data tab names it.
    schema: &'static str,
    /// Drops and creates the table (composite key `a`, `b`), then fills it.
    setup: Vec<String>,
    /// Changes every row's other columns.
    scramble: &'static str,
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

fn login(host: &str, port: u16, user: &str, password: &str, database: &str) -> ConnectParams {
    ConnectParams {
        host: host.into(),
        port,
        user: user.into(),
        password: Some(Secret::new(password)),
        database: database.into(),
        ..Default::default()
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = crate::support::run_dir("spelaion-copy-updates");
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// The table's rows, written with each engine's types and literals: `backslash` is how a
/// string literal holds one backslash, `bytes` the binary literal of `DE AD 00`.
fn setup(
    types: [&str; 9],
    backslash: &str,
    bytes: &str,
    truth: [&str; 2],
    drop: &str,
) -> Vec<String> {
    let [a, b, name, amount, flag, day, at, raw, note] = types;
    let [yes, no] = truth;
    vec![
        drop.to_string(),
        format!(
            "create table {TABLE} (a {a} not null, b {b} not null, name {name}, \
             amount {amount}, flag {flag}, day {day}, seen_at {at}, raw {raw}, note {note}, \
             primary key (a, b))"
        ),
        format!(
            "insert into {TABLE} (a, b, name, amount, flag, day, seen_at, raw, note) values \
             (1, 'x', 'O''Brien {backslash} back', 12.50, {yes}, '2024-03-04', \
             '2024-03-04 10:00:00.5', {bytes}, null)"
        ),
        format!(
            "insert into {TABLE} (a, b, name, amount, flag, day, seen_at, raw, note) values \
             (1, 'y', 'line one\nline two: ñ €', -0.25, {no}, '1999-12-31', \
             '1999-12-31 23:59:59', null, 'kept')"
        ),
        format!(
            "insert into {TABLE} (a, b, name, amount, flag, day, seen_at, raw, note) values \
             (2, 'x', '', 0, null, null, null, null, 'z')"
        ),
    ]
}

async fn cases() -> Vec<Case> {
    let duck = scratch("copy.duckdb");
    engines::create_database(spelaion_config::Engine::Duckdb, &duck).await;
    let sqlite = scratch("copy.sqlite");
    std::fs::write(&sqlite, b"").unwrap();
    let file = |path: &std::path::Path| ConnectParams {
        database: path.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let drop = format!("drop table if exists {TABLE}");
    let scramble = "update copy_updates set name = 'changed', amount = 1, flag = null, \
                    day = '2000-01-01', seen_at = null, raw = null, note = 'changed'";
    let mut out = vec![
        Case {
            label: "sqlite",
            driver: engines::sqlite(),
            params: file(&sqlite),
            schema: "main",
            setup: setup(
                [
                    "integer", "text", "text", "numeric", "boolean", "date", "text", "blob", "text",
                ],
                "\\",
                "X'DEAD00'",
                ["true", "false"],
                &drop,
            ),
            scramble,
        },
        Case {
            label: "duckdb",
            driver: engines::duckdb(),
            params: file(&duck),
            schema: "main",
            setup: setup(
                [
                    "integer",
                    "varchar",
                    "varchar",
                    "decimal(10,2)",
                    "boolean",
                    "date",
                    "timestamp",
                    "blob",
                    "varchar",
                ],
                "\\",
                "from_hex('DEAD00')",
                ["true", "false"],
                &drop,
            ),
            scramble,
        },
    ];
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        out.push(Case {
            label: "postgres",
            driver: engines::postgres(),
            params: login(&host, port, "spelaion", "spelaion", "spelaion"),
            schema: "public",
            setup: setup(
                [
                    "integer",
                    "text",
                    "text",
                    "numeric(10,2)",
                    "boolean",
                    "date",
                    "timestamp(3)",
                    "bytea",
                    "text",
                ],
                "\\",
                "'\\xdead00'",
                ["true", "false"],
                &drop,
            ),
            scramble,
        });
    }
    for (label, var, driver) in [
        ("mysql", "SPELAION_TEST_MYSQL", engines::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", engines::mariadb()),
    ] {
        if let Some((host, port)) = server(var) {
            out.push(Case {
                label,
                driver,
                params: login(&host, port, "spelaion", "spelaion", "spelaion"),
                schema: "spelaion",
                setup: setup(
                    [
                        "int",
                        "varchar(10)",
                        "varchar(100)",
                        "decimal(10,2)",
                        "boolean",
                        "date",
                        "datetime(3)",
                        "varbinary(16)",
                        "text",
                    ],
                    // The session's default mode reads `\\` as one backslash.
                    "\\\\",
                    "X'DEAD00'",
                    ["true", "false"],
                    &drop,
                ),
                scramble,
            });
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        out.push(Case {
            label: "sqlserver",
            driver: engines::sqlserver(),
            params: ConnectParams {
                tls: TlsMode::Require,
                ..login(&host, port, "sa", "Spelaion-test-1", "tempdb")
            },
            schema: "dbo",
            setup: setup(
                [
                    "int",
                    "nvarchar(10)",
                    "nvarchar(100)",
                    "decimal(10,2)",
                    "bit",
                    "date",
                    "datetime2(3)",
                    "varbinary(16)",
                    "nvarchar(max)",
                ],
                "\\",
                "0xDEAD00",
                ["1", "0"],
                &format!("drop table if exists {TABLE}"),
            ),
            scramble,
        });
    }
    out
}

async fn open(case: &Case, store: &Arc<ResultStore>, masking: Option<MaskPolicy>) -> Session {
    let conn = case
        .driver
        .connect(&case.params)
        .await
        .unwrap_or_else(|e| panic!("{}: connecting: {e}", case.label));
    let session = Session::new(
        conn,
        spelaion_dialect::for_engine(case.driver.engine()).expect("a dialect"),
        store.clone(),
    );
    // As the app's tab bar does once connected (its database picker): MySQL's and MariaDB's
    // plugin knows the session's database only once it has been asked. SQLite has none.
    let _ = session.connection().current_database().await;
    match masking {
        Some(policy) => session.with_masking(policy),
        None => session,
    }
}

/// Runs `sql`, panicking on a failure; returns its first result, if any.
async fn run(label: &str, session: &Session, sql: &str) -> Option<ResultId> {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let mut first = None;
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => {
                panic!("{label}: {sql}\n→ {}", error.message)
            }
            CoreEvent::ResultStarted { result, .. } => {
                first.get_or_insert(*result);
            }
            _ => {}
        }
    }
    first
}

/// Every cell of a result, by row.
fn cells(session: &Session, id: ResultId) -> Vec<Vec<Value>> {
    let stored = session.results().get(id).expect("kept");
    let stored = stored.read().unwrap();
    let columns = stored.schema().columns.len();
    (0..stored.rows())
        .map(|row| {
            (0..columns)
                .map(|column| stored.value_at(row, column).expect("a cell"))
                .collect()
        })
        .collect()
}

/// The UPDATE statements of every row and every column of `id`, as `source` decides.
async fn copied(
    session: &Session,
    id: ResultId,
    source: UpdateSource,
) -> Result<String, UpdateCopyError> {
    let update = update_copy_table(session, id, source).await?;
    let stored = session.results().get(id).expect("kept");
    let stored = stored.read().unwrap();
    let rows: Vec<usize> = (0..stored.rows()).collect();
    let columns: Vec<usize> = (0..stored.schema().columns.len()).collect();
    copy_updates(&update, &stored, &rows, &columns)
}

fn table_source(case: &Case) -> UpdateSource {
    UpdateSource::Table {
        schema: case.schema.into(),
        name: TABLE.into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn copied_updates_bring_every_row_back_on_every_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    for case in cases().await {
        let label = case.label;
        let store = Arc::new(ResultStore::new());
        let session = open(&case, &store, None).await;
        for sql in &case.setup {
            run(label, &session, sql).await;
        }
        let original = run(label, &session, QUERY).await.expect("a result");
        let before = cells(&session, original);
        assert_eq!(before.len(), 3, "{label}");

        for source in [
            UpdateSource::Query { sql: QUERY.into() },
            table_source(&case),
        ] {
            let text = copied(&session, original, source.clone())
                .await
                .unwrap_or_else(|e| panic!("{label}: {source:?}: {e}"));
            assert_eq!(
                text.lines().filter(|l| l.starts_with("UPDATE ")).count(),
                3,
                "{label}: {text}"
            );
            // One per row, found by the whole key.
            let quote = |name: &str| {
                spelaion_dialect::for_engine(case.driver.engine())
                    .expect("a dialect")
                    .quote_identifier(name)
            };
            assert!(
                text.contains(&format!("{} = 1 AND {} = ", quote("a"), quote("b"))),
                "{label}: {text}"
            );
            if label == "mysql" || label == "mariadb" {
                // The backslash reads the same in every sql_mode (SDB-161).
                assert!(text.contains("_utf8mb4 X'"), "{label}: {text}");
            }

            run(label, &session, case.scramble).await;
            let changed = run(label, &session, QUERY).await.expect("a result");
            assert_ne!(
                cells(&session, changed),
                before,
                "{label}: the rows changed"
            );

            run(label, &session, &text).await;
            let back = run(label, &session, QUERY).await.expect("a result");
            assert_eq!(cells(&session, back), before, "{label}: {source:?}\n{text}");
        }

        // Only some columns: the UPDATE sets those and nothing else.
        {
            let update = update_copy_table(&session, original, table_source(&case))
                .await
                .unwrap();
            let stored = session.results().get(original).unwrap();
            let stored = stored.read().unwrap();
            let name = stored
                .schema()
                .columns
                .iter()
                .position(|c| c.name == "name")
                .unwrap();
            let note = stored
                .schema()
                .columns
                .iter()
                .position(|c| c.name == "note")
                .unwrap();
            let text = copy_updates(&update, &stored, &[1], &[note, name]).unwrap();
            assert_eq!(text.lines().count(), 2, "{label}: {text}");
            assert!(!text.contains("amount"), "{label}: {text}");
            // Only key columns: nothing to set.
            assert_eq!(
                copy_updates(&update, &stored, &[0], &[0, 1]),
                Err(UpdateCopyError::OnlyKeyColumns),
                "{label}"
            );
        }

        // A join is not one table's rows.
        let join = format!(
            "select t.a, t.b, t.name from {TABLE} t join {TABLE} u on t.a = u.a and t.b = u.b"
        );
        let joined = run(label, &session, &join).await.expect("a result");
        assert_eq!(
            copied(&session, joined, UpdateSource::Query { sql: join }).await,
            Err(UpdateCopyError::NotOneTable),
            "{label}"
        );
        // An expression is not a column of the table either.
        let expression = format!("select a, b, name || 'x' as name from {TABLE}");
        if label != "sqlserver" && label != "mysql" && label != "mariadb" {
            let id = run(label, &session, &expression).await.expect("a result");
            assert_eq!(
                copied(&session, id, UpdateSource::Query { sql: expression }).await,
                Err(UpdateCopyError::NotOneTable),
                "{label}"
            );
        }
        // Without the whole key, rows cannot be found.
        let part = format!("select a, name from {TABLE}");
        let id = run(label, &session, &part).await.expect("a result");
        assert_eq!(
            copied(&session, id, UpdateSource::Query { sql: part }).await,
            Err(UpdateCopyError::KeyNotInResult("b".into())),
            "{label}"
        );

        // Rows read through another session (another tab, the tab after it reconnected).
        let other = open(&case, &store, None).await;
        assert_eq!(
            copied(&other, original, table_source(&case)).await,
            Err(UpdateCopyError::OtherSource),
            "{label}"
        );

        // A table without a primary key.
        run(label, &session, "drop table if exists copy_updates_nokey").await;
        run(
            label,
            &session,
            "create table copy_updates_nokey (a int, b int)",
        )
        .await;
        run(
            label,
            &session,
            "insert into copy_updates_nokey (a, b) values (1, 2)",
        )
        .await;
        let id = run(label, &session, "select a, b from copy_updates_nokey")
            .await
            .expect("a result");
        assert_eq!(
            copied(
                &session,
                id,
                UpdateSource::Query {
                    sql: "select a, b from copy_updates_nokey".into()
                }
            )
            .await,
            Err(UpdateCopyError::NoPrimaryKey("copy_updates_nokey".into())),
            "{label}"
        );
        run(label, &session, "drop table copy_updates_nokey").await;

        // Masked columns (ADR-051): never written from the mask; the other copies hold it.
        let rules = |patterns: &[&str]| {
            MaskPolicy::new(&MaskingRules {
                required: vec![],
                exemptible: patterns.iter().map(|p| p.to_string()).collect(),
                never: vec![],
            })
        };
        let masked = session_masked(&case, &store, rules(&["note"])).await;
        let id = run(label, &masked, QUERY).await.expect("a result");
        assert_eq!(
            copied(&masked, id, table_source(&case)).await,
            Err(UpdateCopyError::Masked("note".into())),
            "{label}"
        );
        {
            let update = update_copy_table(&masked, id, table_source(&case))
                .await
                .unwrap();
            let stored = masked.results().get(id).unwrap();
            let stored = stored.read().unwrap();
            let columns = &stored.schema().columns;
            let at = |n: &str| columns.iter().position(|c| c.name == n).unwrap();
            // Left out of the selection, the other columns copy.
            let text = copy_updates(&update, &stored, &[0, 1], &[at("name")]).unwrap();
            assert!(!text.contains("note"), "{label}: {text}");
            // "Copy special…" copies the mask, and a NULL stays NULL.
            let delimited = copy_special(
                &stored,
                &[0, 1],
                &[at("note")],
                &CopySpecial {
                    null_text: "NULL".into(),
                    ..CopySpecial::default()
                },
            )
            .unwrap();
            assert_eq!(delimited, format!("note\nNULL\n{MASK}\n"), "{label}");
        }
        let masked_key = session_masked(&case, &store, rules(&["b"])).await;
        let id = run(label, &masked_key, QUERY).await.expect("a result");
        assert_eq!(
            copied(&masked_key, id, table_source(&case)).await,
            Err(UpdateCopyError::Masked("b".into())),
            "{label}"
        );

        run(label, &session, &format!("drop table {TABLE}")).await;
    }
}

async fn session_masked(case: &Case, store: &Arc<ResultStore>, policy: MaskPolicy) -> Session {
    open(case, store, Some(policy)).await
}

/// ClickHouse declares no transactions, so the app applies no edits there and copies none.
#[tokio::test(flavor = "multi_thread")]
async fn engines_without_edits_refuse_copy_as_update() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server("SPELAION_TEST_CLICKHOUSE") else {
        return;
    };
    let case = Case {
        label: "clickhouse",
        driver: engines::clickhouse(),
        params: ConnectParams {
            tls: TlsMode::Disable,
            ..login(&host, port, "spelaion", "spelaion", "spelaion")
        },
        schema: "spelaion",
        setup: vec![],
        scramble: "",
    };
    let store = Arc::new(ResultStore::new());
    let session = open(&case, &store, None).await;
    let id = run("clickhouse", &session, "select 1 as a, 'x' as b")
        .await
        .expect("a result");
    assert_eq!(
        copied(
            &session,
            id,
            UpdateSource::Query {
                sql: "select 1 as a".into()
            }
        )
        .await,
        Err(UpdateCopyError::NoEditing)
    );
    assert_eq!(
        copied(&session, id, table_source(&case)).await,
        Err(UpdateCopyError::NoEditing)
    );
}
