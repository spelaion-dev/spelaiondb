//! The cell panel's value pickers on every engine: SQLite and DuckDB always, PostgreSQL,
//! MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without them they are
//! skipped, which does not count as tested).
//!
//! Each picked value is applied as a table edit (one UPDATE by primary key) and the page is
//! read again: the cell must show exactly the text the picker wrote, the text the drivers'
//! type zoos compare with each server's own. The foreign key picker searches a referenced
//! table of a million rows a page at a time.

use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use spelaion_core::{
    CellInput, ChoiceSource, CoreEvent, KEY_PAGE_ROWS, PickedTime, ResultId, ResultStore,
    RowChange, Session, TablePage, TemporalKind, TemporalStep, ValuePicker, apply_edits,
    edit_statements, picked_choice_text, picked_time_text, referenced_page_sql, search_referenced,
    table_page_sql, value_picker,
};
use spelaion_driver_api::{
    ColumnInfo, ConnectParams, Driver, ForeignKeyInfo, ObjectInfo, ObjectKind, Secret, TableInfo,
    TlsMode, Value,
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

struct Engine {
    label: &'static str,
    session: Session,
    schema: &'static str,
}

async fn engines() -> Vec<Engine> {
    let memory = || ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let mut out = vec![
        Engine {
            label: "sqlite",
            session: session(&spelaion_driver_sqlite::SqliteDriver::default(), memory()).await,
            schema: "main",
        },
        Engine {
            label: "duckdb",
            session: session(&spelaion_driver_duckdb::DuckdbDriver::default(), memory()).await,
            schema: "main",
        },
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
        out.push(Engine {
            label: "postgres",
            session: session(&spelaion_driver_postgres::PostgresDriver, params).await,
            schema: "public",
        });
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
            out.push(Engine {
                label,
                session: session(&driver, params).await,
                schema: "spelaion",
            });
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push(Engine {
            label: "sqlserver",
            session: session(&spelaion_driver_sqlserver::SqlServerDriver, params).await,
            schema: "dbo",
        });
    }
    out
}

/// Runs each statement on its own; panics on a failure unless `tolerate`. Returns the result
/// of the last statement that produced one.
async fn run(
    label: &str,
    session: &Session,
    statements: &[String],
    tolerate: bool,
) -> Option<ResultId> {
    let mut last = None;
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            match event {
                CoreEvent::StatementFailed { error, .. } if !tolerate => {
                    let shown: String = statement.chars().take(300).collect();
                    panic!("{label}: {shown}\n→ {}", error.message)
                }
                CoreEvent::ResultStarted { result, .. } => last = Some(*result),
                _ => {}
            }
        }
    }
    last
}

/// A page of `table` ordered by its key, loaded as a table data tab loads it.
async fn page(engine: &Engine, table: &str) -> ResultId {
    let session = &engine.session;
    let sql = table_page_sql(
        session.dialect().as_ref(),
        &TablePage {
            schema: engine.schema.into(),
            name: table.into(),
            filter: String::new(),
            order_by: session.dialect().quote_identifier("id"),
            limit: 100,
            offset: 0,
        },
    )
    .unwrap_or_else(|e| panic!("{}: page: {e}", engine.label));
    run(engine.label, session, &[sql], false)
        .await
        .unwrap_or_else(|| panic!("{}: no page", engine.label))
}

/// Each column of the first row of a page: its name and its text as the grid shows it.
fn first_row(session: &Session, result: ResultId) -> Vec<(String, Option<String>)> {
    let stored = session.results().get(result).expect("result");
    let stored = stored.read().unwrap();
    let batch = stored.window(0, 1).into_iter().next().expect("a row");
    stored
        .schema()
        .columns
        .iter()
        .enumerate()
        .map(|(i, meta)| {
            let text = match Value::from_array(batch.column(i).as_ref(), 0, meta.logical) {
                Some(Value::Null) | None => None,
                Some(value) => Some(value.to_string()),
            };
            (meta.name.clone(), text)
        })
        .collect()
}

/// Sets `values` (column name → text) on the first row of `table` as a table data tab does:
/// one UPDATE by primary key, applied in a transaction. Then reads the row back.
async fn edit_first_row(
    engine: &Engine,
    table: &str,
    values: &[(String, String)],
) -> Vec<(String, Option<String>)> {
    let label = engine.label;
    let session = &engine.session;
    let result = page(engine, table).await;
    let described = session
        .connection()
        .describe_table(engine.schema, table)
        .await
        .unwrap_or_else(|e| panic!("{label}: describe: {e}"));
    let statements = {
        let stored = session.results().get(result).expect("result");
        let stored = stored.read().unwrap();
        let columns = &stored.schema().columns;
        let change = RowChange::Update {
            row: 0,
            values: values
                .iter()
                .map(|(name, text)| {
                    let i = columns
                        .iter()
                        .position(|c| &c.name == name)
                        .unwrap_or_else(|| panic!("{label}: no column {name}"));
                    (i, CellInput::Text(text.clone()))
                })
                .collect(),
        };
        edit_statements(session.dialect().as_ref(), &described, &stored, &[change])
            .unwrap_or_else(|e| panic!("{label}: {e}"))
    };
    assert_eq!(
        apply_edits(session, &statements).await,
        Ok(1),
        "{label}: {statements:?}"
    );
    first_row(session, page(engine, table).await)
}

/// The text of `column` in a row read back.
fn cell<'a>(row: &'a [(String, Option<String>)], column: &str) -> Option<&'a str> {
    row.iter()
        .find(|(name, _)| name == column)
        .and_then(|(_, text)| text.as_deref())
}

/// The date and time columns of each engine.
fn temporal_columns(label: &str) -> &'static [(&'static str, &'static str)] {
    match label {
        "postgres" => &[
            ("d", "date"),
            ("t", "time"),
            ("t0", "time(0)"),
            ("ts", "timestamp"),
            ("ts3", "timestamp(3)"),
            ("tz", "timestamptz"),
            ("tz2", "timestamptz(2)"),
        ],
        "mysql" | "mariadb" => &[
            ("d", "date"),
            ("dt", "datetime"),
            ("dt6", "datetime(6)"),
            ("ts3", "timestamp(3) null"),
            ("t", "time"),
            ("t3", "time(3)"),
        ],
        "sqlserver" => &[
            ("d", "date"),
            ("t", "time"),
            ("t3", "time(3)"),
            ("dt", "datetime"),
            ("sdt", "smalldatetime"),
            ("dt2", "datetime2"),
            ("dt23", "datetime2(3)"),
            ("dto", "datetimeoffset"),
            ("dto2", "datetimeoffset(2)"),
        ],
        "sqlite" => &[("d", "DATE"), ("dt", "DATETIME"), ("t", "TIME")],
        "duckdb" => &[
            ("d", "DATE"),
            ("t", "TIME"),
            ("ts", "TIMESTAMP"),
            ("tsms", "TIMESTAMP_MS"),
            ("tss", "TIMESTAMP_S"),
            ("tsns", "TIMESTAMP_NS"),
            ("tz", "TIMESTAMPTZ"),
        ],
        other => panic!("no columns for {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn picked_dates_and_times_read_back_as_written_on_every_engine() {
    const TABLE: &str = "vp_times";
    for engine in engines().await {
        let label = engine.label;
        let session = &engine.session;
        let columns = temporal_columns(label);
        run(label, session, &[format!("DROP TABLE {TABLE}")], true).await;
        let definitions: Vec<String> = columns
            .iter()
            .map(|(name, native)| format!("{name} {native}"))
            .collect();
        run(
            label,
            session,
            &[
                format!(
                    "CREATE TABLE {TABLE} (id INT PRIMARY KEY, {})",
                    definitions.join(", ")
                ),
                format!("INSERT INTO {TABLE} (id) VALUES (1)"),
            ],
            false,
        )
        .await;

        // Two rounds: a late time with a full fraction (seven digits where the column keeps
        // them), then an early one with a short fraction and an offset that moves a column with
        // a time zone into the previous day, written in UTC or with its offset.
        for (time, fraction, offset) in [((23, 59, 58), "1234567", 0), ((1, 30, 0), "5", 120)] {
            let mut picks = Vec::new();
            for (name, native) in columns {
                let picker = value_picker(session, engine.schema, TABLE, name)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {name}: {e}"));
                let Some(ValuePicker::Temporal(picker)) = picker else {
                    panic!("{label}: {name} ({native}) has no date and time picker: {picker:?}")
                };
                let digits = usize::from(picker.digits);
                let (hour, minute, second) = time;
                let picked = PickedTime {
                    year: 2026,
                    month: 1,
                    day: 1,
                    hour,
                    minute,
                    second: if picker.step == TemporalStep::Minutes {
                        0
                    } else {
                        second
                    },
                    fraction: fraction[..fraction.len().min(digits)].to_string(),
                    offset_minutes: offset,
                };
                let text = picked_time_text(&picker, &picked)
                    .unwrap_or_else(|e| panic!("{label}: {name}: {e}"));
                if picker.kind == TemporalKind::TimestampTz && offset == 120 {
                    if picker.keeps_offset {
                        assert!(
                            text.starts_with("2026-01-01 01:30:00") && text.ends_with(" +02:00"),
                            "{label}: {text}"
                        );
                    } else {
                        assert!(text.starts_with("2025-12-31 23:30:00"), "{label}: {text}");
                    }
                }
                picks.push((name.to_string(), text));
            }
            let row = edit_first_row(&engine, TABLE, &picks).await;
            for (name, text) in &picks {
                assert_eq!(
                    cell(&row, name),
                    Some(text.as_str()),
                    "{label}: {name} was written as {text:?}"
                );
            }
        }
        run(label, session, &[format!("DROP TABLE {TABLE}")], true).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn allowed_values_come_from_the_catalog_on_every_engine() {
    const TABLE: &str = "vp_choices";
    for engine in engines().await {
        let label = engine.label;
        let session = &engine.session;
        let status = "status VARCHAR(10) CHECK (status IN ('new', 'paid'))";
        let level = "level INT CHECK (level IN (1, 2, 3))";
        // (column, its allowed values, how they are allowed, what is picked)
        let mut expected: Vec<(&str, Vec<&str>, ChoiceSource, Vec<&str>)> = vec![
            (
                "status",
                vec!["new", "paid"],
                ChoiceSource::Check,
                vec!["paid"],
            ),
            ("level", vec!["1", "2", "3"], ChoiceSource::Check, vec!["3"]),
        ];
        let mood = (
            "mood",
            vec!["sad", "o'k", "happy"],
            ChoiceSource::Enum,
            vec!["o'k"],
        );
        let (setup, columns): (Vec<String>, Vec<String>) = match label {
            "postgres" => {
                expected.push(mood.clone());
                expected.push(("mood_d", mood.1.clone(), ChoiceSource::Enum, vec!["happy"]));
                (
                    vec![
                        "DROP DOMAIN IF EXISTS vp_mood_d".into(),
                        "DROP TYPE IF EXISTS vp_mood".into(),
                        "CREATE TYPE vp_mood AS ENUM ('sad', 'o''k', 'happy')".into(),
                        "CREATE DOMAIN vp_mood_d AS vp_mood".into(),
                    ],
                    vec![
                        status.into(),
                        level.into(),
                        "mood vp_mood".into(),
                        "mood_d vp_mood_d".into(),
                    ],
                )
            }
            "mysql" | "mariadb" => {
                expected.push(mood.clone());
                expected.push((
                    "tags",
                    vec!["x", "y", "z"],
                    ChoiceSource::Set,
                    vec!["z", "x"],
                ));
                (
                    vec![],
                    vec![
                        status.into(),
                        level.into(),
                        "mood ENUM('sad', 'o''k', 'happy')".into(),
                        "tags SET('x', 'y', 'z')".into(),
                    ],
                )
            }
            "duckdb" => {
                expected.push(mood.clone());
                (
                    vec![
                        "DROP TYPE IF EXISTS vp_mood".into(),
                        "CREATE TYPE vp_mood AS ENUM ('sad', 'o''k', 'happy')".into(),
                    ],
                    vec![status.into(), level.into(), "mood vp_mood".into()],
                )
            }
            _ => (vec![], vec![status.into(), level.into()]),
        };
        run(label, session, &[format!("DROP TABLE {TABLE}")], true).await;
        run(label, session, &setup, false).await;
        run(
            label,
            session,
            &[
                format!(
                    "CREATE TABLE {TABLE} (id INT PRIMARY KEY, {})",
                    columns.join(", ")
                ),
                format!("INSERT INTO {TABLE} (id) VALUES (1)"),
            ],
            false,
        )
        .await;

        let mut picks = Vec::new();
        for (name, values, source, chosen) in &expected {
            let picker = value_picker(session, engine.schema, TABLE, name)
                .await
                .unwrap_or_else(|e| panic!("{label}: {name}: {e}"));
            let Some(ValuePicker::Choice(picker)) = picker else {
                panic!("{label}: {name} has no list of values: {picker:?}")
            };
            // SQL Server keeps `IN ('new', 'paid')` as `[status]='paid' OR [status]='new'`:
            // its order is the catalog's.
            let mut listed = picker.values.clone();
            let mut wanted: Vec<String> = values.iter().map(|v| v.to_string()).collect();
            if label == "sqlserver" {
                listed.sort();
                wanted.sort();
            }
            assert_eq!(listed, wanted, "{label}: {name}");
            assert_eq!(picker.source, *source, "{label}: {name}");
            let chosen: Vec<String> = chosen.iter().map(|v| v.to_string()).collect();
            let text = picked_choice_text(&picker, &chosen)
                .unwrap_or_else(|e| panic!("{label}: {name}: {e}"));
            picks.push((name.to_string(), text));
        }
        // A plain column takes no picker.
        assert_eq!(
            value_picker(session, engine.schema, TABLE, "id")
                .await
                .unwrap(),
            None,
            "{label}"
        );
        let row = edit_first_row(&engine, TABLE, &picks).await;
        for (name, text) in &picks {
            assert_eq!(cell(&row, name), Some(text.as_str()), "{label}: {name}");
        }
        if label == "mysql" || label == "mariadb" {
            assert_eq!(cell(&row, "tags"), Some("x,z"), "{label}");
        }
        run(label, session, &[format!("DROP TABLE {TABLE}")], true).await;
        if label == "postgres" {
            run(
                label,
                session,
                &["DROP DOMAIN vp_mood_d".into(), "DROP TYPE vp_mood".into()],
                true,
            )
            .await;
        }
    }
}

/// The statement that fills `vp_ref` with ids 1 to 1,000,000 named `item <id>`.
fn million_rows(label: &str) -> String {
    match label {
        "postgres" => "INSERT INTO vp_ref (id, name) SELECT g, 'item ' || g \
                       FROM generate_series(1, 1000000) AS g"
            .into(),
        "duckdb" => "INSERT INTO vp_ref (id, name) SELECT range, 'item ' || range \
                     FROM range(1, 1000001)"
            .into(),
        "sqlite" => "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s \
                     WHERE n < 1000000) INSERT INTO vp_ref (id, name) \
                     SELECT n, 'item ' || n FROM s"
            .into(),
        _ => {
            let digit = "(SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL \
                         SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 \
                         UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9)";
            let tables: Vec<String> = (0..6).map(|i| format!("{digit} AS d{i}")).collect();
            let number: Vec<String> = (0..6)
                .map(|i| format!("{} * d{i}.d", 10_u32.pow(i)))
                .collect();
            format!(
                "INSERT INTO vp_ref (id, name) SELECT n, CONCAT('item ', n) FROM \
                 (SELECT {} + 1 AS n FROM {}) AS numbers",
                number.join(" + "),
                tables.join(" CROSS JOIN ")
            )
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_key_picks_from_a_million_rows_a_page_at_a_time() {
    for engine in engines().await {
        let label = engine.label;
        let session = &engine.session;
        let drop = vec![
            "DROP TABLE vp_child".to_string(),
            "DROP TABLE vp_ref".into(),
        ];
        run(label, session, &drop, true).await;
        let started = Instant::now();
        run(
            label,
            session,
            &[
                "CREATE TABLE vp_ref (id INT PRIMARY KEY, name VARCHAR(40), code INT)".into(),
                million_rows(label),
                "CREATE TABLE vp_child (id INT PRIMARY KEY, ref_id INT, note VARCHAR(10), \
                 FOREIGN KEY (ref_id) REFERENCES vp_ref (id))"
                    .into(),
                "INSERT INTO vp_child (id, ref_id) VALUES (1, 5)".into(),
            ],
            false,
        )
        .await;
        eprintln!("{label}: a million rows in {:?}", started.elapsed());

        let picker = value_picker(session, engine.schema, "vp_child", "ref_id")
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let Some(ValuePicker::ForeignKey(key)) = picker else {
            panic!("{label}: ref_id has no foreign key picker: {picker:?}")
        };
        assert_eq!(key.table, "vp_ref", "{label}");
        assert_eq!(key.columns, ["ref_id"], "{label}");
        assert_eq!(key.referenced_columns, ["id"], "{label}");
        // Not part of a key: no search.
        assert!(
            search_referenced(session, engine.schema, "vp_child", "note", "", 0)
                .await
                .is_err(),
            "{label}"
        );

        let search = |text: &'static str, offset: u64| async move {
            let started = Instant::now();
            let page =
                search_referenced(session, engine.schema, "vp_child", "ref_id", text, offset)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
            eprintln!(
                "{label}: search {text:?} at {offset}: {} rows in {:?}",
                page.rows.len(),
                started.elapsed()
            );
            page
        };
        // The first page: the key and the text column, never the whole table.
        let first = search("", 0).await;
        assert_eq!(first.columns, ["id", "name"], "{label}");
        assert_eq!(first.key_columns, 1, "{label}");
        assert_eq!(first.rows.len() as u64, KEY_PAGE_ROWS, "{label}");
        assert!(first.has_more, "{label}");
        assert_eq!(
            first.rows[0],
            [Some("1".to_string()), Some("item 1".to_string())],
            "{label}"
        );
        // The last page.
        let last = search("", 1_000_000 - KEY_PAGE_ROWS).await;
        assert_eq!(last.rows.len() as u64, KEY_PAGE_ROWS, "{label}");
        assert!(!last.has_more, "{label}");
        assert_eq!(
            last.rows.last().unwrap()[0].as_deref(),
            Some("1000000"),
            "{label}"
        );
        // A search finds one row among a million.
        let found = search("item 777777", 0).await;
        assert_eq!(
            found.rows,
            [[Some("777777".to_string()), Some("item 777777".to_string())]],
            "{label}"
        );
        assert!(!found.has_more, "{label}");
        // Its own wildcards are taken literally.
        assert!(search("item 7%7", 0).await.rows.is_empty(), "{label}");
        // A key searched by its digits: ids containing them, in the key's order.
        let digits = search("99999", 0).await;
        assert_eq!(digits.rows[0][0].as_deref(), Some("99999"), "{label}");

        // The picked row's key is written to the referencing column.
        let picked = found.rows[0][0].clone().unwrap();
        let row = edit_first_row(&engine, "vp_child", &[("ref_id".into(), picked.clone())]).await;
        assert_eq!(cell(&row, "ref_id"), Some(picked.as_str()), "{label}");
        run(label, session, &drop, true).await;
    }
}

#[test]
fn the_key_search_is_a_checked_page_of_the_referenced_table() {
    let column = |name: &str, native: &str| ColumnInfo {
        name: name.into(),
        native_type: native.into(),
        nullable: true,
        default: None,
        comment: None,
    };
    let referenced = TableInfo {
        object: ObjectInfo {
            schema: "main".into(),
            name: "customers".into(),
            kind: ObjectKind::Table,
            estimated_rows: None,
            comment: None,
        },
        columns: vec![
            column("id", "INTEGER"),
            column("photo", "BLOB"),
            column("name", "TEXT"),
            column("email", "VARCHAR(80)"),
            column("city", "TEXT"),
            column("notes", "TEXT"),
        ],
        primary_key: vec!["id".into()],
        indexes: vec![],
        foreign_keys: vec![],
        constraints: vec![],
    };
    let key = ForeignKeyInfo {
        name: "fk_0".into(),
        columns: vec!["customer_id".into()],
        referenced_schema: "main".into(),
        referenced_table: "customers".into(),
        referenced_columns: vec!["id".into()],
    };
    let dialect = spelaion_driver_sqlite::SqliteDialect;
    let page = referenced_page_sql(&dialect, &key, &referenced, "  ", 100).unwrap();
    assert_eq!(page.columns, ["id", "name", "email", "city"]);
    assert_eq!(
        page.sql,
        "SELECT id, name, email, city\nFROM customers\nORDER BY\nid\n\nLIMIT 51 OFFSET 100"
    );
    // What is searched is a literal: it cannot end the statement or read another table.
    let page = referenced_page_sql(
        &dialect,
        &key,
        &referenced,
        "x') UNION SELECT * FROM secrets; --",
        0,
    )
    .unwrap();
    assert!(
        page.sql
            .contains("CAST(id AS TEXT) LIKE '%x'') UNION SELECT * FROM secrets; --%' ESCAPE '!'"),
        "{}",
        page.sql
    );
    assert!(
        page.sql
            .contains("name LIKE '%x'') UNION SELECT * FROM secrets; --%' ESCAPE '!'"),
        "{}",
        page.sql
    );
    // SQL Server pages with OFFSET … FETCH.
    let dialect = spelaion_driver_sqlserver::SqlServerDialect;
    let page = referenced_page_sql(&dialect, &key, &referenced, "a", 50).unwrap();
    assert!(
        page.sql
            .ends_with("ORDER BY\nid\n\nOFFSET 50 ROWS FETCH NEXT 51 ROWS ONLY"),
        "{}",
        page.sql
    );
}
