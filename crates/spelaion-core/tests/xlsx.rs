//! Excel in and out (SDB-390) on every engine whose exports stream (ADR-034) and that imports
//! (ADR-038): SQLite and DuckDB always; PostgreSQL, MySQL, MariaDB and SQL Server with their
//! `SPELAION_TEST_*` variables (without them they are skipped, which does not count as
//! tested).
//!
//! A table of the common types is exported as XLSX through a session of its own, the workbook
//! is imported into a new table through the import's preview and plan, and both tables are
//! compared value by value. A masked column is the mask in the workbook. A million-row export
//! holds one batch at a time and goes on in a second sheet past Excel's row limit; explorer
//! tables become one workbook, a sheet each.

use crate::engines;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{MaskingRules, Safeguards};
use spelaion_core::{
    ColumnMapping, CoreEvent, CsvOptions, ExportFormat, FileNamePattern, IfExists, ImportError,
    ImportFormat, ImportRequest, ImportSource, ImportTarget, JobCancel, JsonOptions, MASK,
    MaskPolicy, ParseOptions, ResultId, ResultStore, Session, SqlOptions, StreamExport,
    TableSource, TextEncoding, export_query, export_table, export_tables, guess_options,
    plan_import, preview, run_import,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

const NONE: Safeguards = Safeguards {
    confirm_execution: false,
    confirm_data_changes: false,
    close_idle_after: None,
    close_idle_transaction_after: None,
    manual_commit: false,
    smart_commit: false,
};

struct Case {
    label: &'static str,
    driver: Arc<dyn Driver>,
    params: ConnectParams,
    schema: &'static str,
    /// id, name, amount, ratio, flag, day, seen_at, note.
    types: [&'static str; 8],
    backslash: &'static str,
    truth: [&'static str; 2],
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

fn scratch(name: &str) -> PathBuf {
    let dir = crate::support::run_dir("spelaion-xlsx-tests");
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

async fn cases() -> Vec<Case> {
    let duck = scratch("xlsx.duckdb");
    engines::create_database(spelaion_config::Engine::Duckdb, &duck).await;
    let sqlite = scratch("xlsx.sqlite");
    std::fs::write(&sqlite, b"").unwrap();
    let file = |path: &Path| ConnectParams {
        database: path.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let mut out = vec![
        Case {
            label: "sqlite",
            driver: engines::sqlite(),
            params: file(&sqlite),
            schema: "main",
            types: [
                "integer", "text", "real", "real", "boolean", "date", "text", "text",
            ],
            backslash: "\\",
            truth: ["true", "false"],
        },
        Case {
            label: "duckdb",
            driver: engines::duckdb(),
            params: file(&duck),
            schema: "main",
            types: [
                "integer",
                "varchar",
                "decimal(10,2)",
                "double",
                "boolean",
                "date",
                "timestamp",
                "varchar",
            ],
            backslash: "\\",
            truth: ["true", "false"],
        },
    ];
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        out.push(Case {
            label: "postgres",
            driver: engines::postgres(),
            params: login(&host, port, "spelaion", "spelaion", "spelaion"),
            schema: "public",
            types: [
                "integer",
                "text",
                "numeric(10,2)",
                "double precision",
                "boolean",
                "date",
                "timestamp(3)",
                "text",
            ],
            backslash: "\\",
            truth: ["true", "false"],
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
                types: [
                    "int",
                    "varchar(100)",
                    "decimal(10,2)",
                    "double",
                    "boolean",
                    "date",
                    "datetime(3)",
                    "text",
                ],
                backslash: "\\\\",
                truth: ["true", "false"],
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
            types: [
                "int",
                "nvarchar(100)",
                "decimal(10,2)",
                "float",
                "bit",
                "date",
                "datetime2(3)",
                "nvarchar(max)",
            ],
            backslash: "\\",
            truth: ["1", "0"],
        });
    }
    out
}

fn setup(case: &Case, table: &str) -> Vec<String> {
    let [id, name, amount, ratio, flag, day, at, note] = case.types;
    let [yes, no] = case.truth;
    let backslash = case.backslash;
    vec![
        format!("drop table if exists {table}"),
        format!(
            "create table {table} (id {id} not null primary key, name {name}, amount {amount}, \
             ratio {ratio}, flag {flag}, day {day}, seen_at {at}, note {note})"
        ),
        format!(
            "insert into {table} (id, name, amount, ratio, flag, day, seen_at, note) values \
             (1, 'O''Brien {backslash} back <b>&amp; _x0041_', 12.50, 1.5, {yes}, '2024-03-04', \
             '2024-03-04 10:00:00.5', null)"
        ),
        format!(
            "insert into {table} (id, name, amount, ratio, flag, day, seen_at, note) values \
             (2, 'line one\nline two: ñ €', -0.25, -22500000000.25, {no}, '1999-12-31', \
             '1999-12-31 23:59:59', 'kept')"
        ),
        format!(
            "insert into {table} (id, name, amount, ratio, flag, day, seen_at, note) values \
             (3, '', 0, 0, null, null, null, 'z')"
        ),
    ]
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

fn xlsx(batch_size: usize) -> StreamExport {
    StreamExport {
        format: ExportFormat::Xlsx,
        header: true,
        csv: CsvOptions::default(),
        encoding: TextEncoding::Utf8,
        gzip: false,
        sql: SqlOptions::default(),
        json: JsonOptions::default(),
        columns: None,
        batch_size,
    }
}

/// The options an XLSX import is read with: empty text stays text (only a missing cell is NULL).
fn sheet_options(sheet: Option<&str>) -> ParseOptions {
    ParseOptions {
        format: ImportFormat::Xlsx,
        empty_as_null: false,
        sheet: sheet.map(str::to_string),
        ..ParseOptions::default()
    }
}

/// A value as both tables can be compared: numbers as numbers, booleans as `true`/`false`
/// whatever the engine stores them as, a timestamp's fraction without trailing zeros.
fn normal(column: usize, value: &Value) -> String {
    if matches!(value, Value::Null) {
        return "NULL".into();
    }
    let text = value.to_string();
    match column {
        // id, amount, ratio
        0 | 2 | 3 => match text.parse::<f64>() {
            Ok(x) => format!("{x}"),
            Err(_) => text,
        },
        4 => match text.as_str() {
            "1" | "true" => "true".into(),
            "0" | "false" => "false".into(),
            _ => text,
        },
        6 => match text.split_once('.') {
            Some((whole, fraction)) => {
                let fraction = fraction.trim_end_matches('0');
                if fraction.is_empty() {
                    whole.to_string()
                } else {
                    format!("{whole}.{fraction}")
                }
            }
            None => text,
        },
        _ => text,
    }
}

fn rows(session: &Session, id: ResultId) -> Vec<Vec<String>> {
    let stored = session.results().get(id).expect("kept");
    let stored = stored.read().unwrap();
    let width = stored.schema().columns.len();
    (0..stored.rows())
        .map(|row| {
            (0..width)
                .map(|c| normal(c, &stored.value_at(row, c).unwrap()))
                .collect()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_table_goes_through_excel_and_back_on_every_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    let pid = std::process::id();
    for case in cases().await {
        let label = case.label;
        let table = format!("xlsx_source_{pid}");
        let back = format!("xlsx_back_{pid}");
        let store = Arc::new(ResultStore::new());
        let work = open(&case, &store, None).await;
        for sql in setup(&case, &table) {
            run(label, &work, &sql).await;
        }
        run(label, &work, &format!("drop table if exists {back}")).await;
        let original = run(label, &work, &format!("select * from {table} order by id"))
            .await
            .expect("a result");
        let want = rows(&work, original);

        // Exported through a session of its own, two rows per batch.
        let path = scratch(&format!("{label}-round-trip.xlsx"));
        let exporting = open(&case, &store, None).await;
        let source = TableSource {
            schema: case.schema.into(),
            name: table.clone(),
            filter: String::new(),
            order_by: "id".into(),
        };
        let summary = export_table(
            &exporting,
            &source,
            &path,
            &xlsx(2),
            &JobCancel::new(),
            &|_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: export: {e}"));
        assert_eq!(summary.rows, 3, "{label}");
        assert_eq!(summary.peak_rows, 2, "{label}: one batch at a time");

        // Imported into a new table, as the import dialog does.
        let options = sheet_options(None);
        let shown = {
            let (path, engine) = (path.clone(), work.dialect().name());
            let options = options.clone();
            tokio::task::spawn_blocking(move || {
                preview(&ImportSource::File(path), Some(&options), 1_000, engine)
            })
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("{label}: preview: {e}"))
        };
        assert_eq!(shown.sheets, vec![table.clone()], "{label}");
        let names: Vec<&str> = shown.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "id", "name", "amount", "ratio", "flag", "day", "seen_at", "note"
            ],
            "{label}"
        );
        let request = ImportRequest {
            source: ImportSource::File(path.clone()),
            options,
            target: ImportTarget::New {
                schema: case.schema.into(),
                table: back.clone(),
                if_exists: IfExists::Fail,
            },
            columns: shown
                .columns
                .iter()
                .map(|c| ColumnMapping::New {
                    name: c.name.clone(),
                    sql_type: c.sql_type.clone(),
                })
                .collect(),
            commit_every: 0,
        };
        let plan = plan_import(&work, &request, &NONE, false)
            .await
            .unwrap_or_else(|e| panic!("{label}: plan: {e}"));
        assert!(plan.creates_table, "{label}");
        let imported = run_import(
            &work,
            &request,
            &NONE,
            false,
            true,
            &JobCancel::new(),
            &|_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: import: {e}\n{plan:?}"));
        assert_eq!(imported.rows, 3, "{label}");
        let again = run(label, &work, &format!("select * from {back} order by id"))
            .await
            .expect("a result");
        assert_eq!(rows(&work, again), want, "{label}: {plan:?}");

        // A masked column is the mask in the workbook; NULL stays NULL.
        let masking = MaskPolicy::new(&MaskingRules {
            required: vec![],
            exemptible: vec!["note".into()],
            never: vec![],
        });
        let masked = scratch(&format!("{label}-masked.xlsx"));
        let session = open(&case, &store, Some(masking)).await;
        export_table(
            &session,
            &source,
            &masked,
            &xlsx(2),
            &JobCancel::new(),
            &|_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: masked export: {e}"));
        let seen = {
            let masked = masked.clone();
            tokio::task::spawn_blocking(move || {
                preview(
                    &ImportSource::File(masked),
                    Some(&sheet_options(None)),
                    1_000,
                    "sqlite",
                )
            })
            .await
            .unwrap()
            .unwrap()
        };
        let notes: Vec<Option<&str>> = seen.rows.iter().map(|r| r[7].as_deref()).collect();
        assert_eq!(notes, [None, Some(MASK), Some(MASK)], "{label}");
        run(label, &work, &format!("drop table {back}")).await;
        run(label, &work, &format!("drop table {table}")).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_million_rows_hold_one_batch_and_go_on_in_a_new_sheet() {
    let _turn = crate::serial::file_turn(module_path!());
    const ROWS: u64 = 1_048_600;
    let path = scratch("million.sqlite");
    std::fs::write(&path, b"").unwrap();
    let case = Case {
        label: "sqlite",
        driver: engines::sqlite(),
        params: ConnectParams {
            database: path.to_string_lossy().into_owned(),
            ..Default::default()
        },
        schema: "main",
        types: [""; 8],
        backslash: "\\",
        truth: ["1", "0"],
    };
    let store = Arc::new(ResultStore::new());
    let session = open(&case, &store, None).await;
    let book = scratch("million.xlsx");
    let sql = format!(
        "with recursive n(i) as (select 1 union all select i + 1 from n where i < {ROWS}) \
         select i as id, 'row ' || i as name from n"
    );
    let summary = export_query(
        &session,
        &sql,
        &NONE,
        false,
        &book,
        &xlsx(spelaion_core::DEFAULT_BATCH_ROWS),
        &JobCancel::new(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(summary.rows, ROWS);
    // Never more than one batch held, whatever the row count.
    assert_eq!(summary.peak_rows, spelaion_core::DEFAULT_BATCH_ROWS as u64);
    let read = |sheet: Option<&'static str>| {
        let book = book.clone();
        move || {
            preview(
                &ImportSource::File(book),
                Some(&sheet_options(sheet)),
                100_000,
                "sqlite",
            )
        }
    };
    let first = tokio::task::spawn_blocking(read(None))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.sheets, ["million", "million (2)"]);
    assert!(first.more);
    assert_eq!(first.rows[0], [Some("1".into()), Some("row 1".into())]);
    // Excel's 1,048,576 rows are the header and 1,048,575 rows: 25 are left for the second.
    let second = tokio::task::spawn_blocking(read(Some("million (2)")))
        .await
        .unwrap()
        .unwrap();
    assert!(!second.more);
    assert_eq!(second.sampled, 25);
    assert_eq!(
        second.rows[0],
        [Some("1048576".into()), Some("row 1048576".into())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn explorer_tables_become_one_workbook() {
    let _turn = crate::serial::file_turn(module_path!());
    let path = scratch("tables.sqlite");
    std::fs::write(&path, b"").unwrap();
    let case = Case {
        label: "sqlite",
        driver: engines::sqlite(),
        params: ConnectParams {
            database: path.to_string_lossy().into_owned(),
            ..Default::default()
        },
        schema: "main",
        types: [""; 8],
        backslash: "\\",
        truth: ["1", "0"],
    };
    let store = Arc::new(ResultStore::new());
    let session = open(&case, &store, None).await;
    for sql in [
        "create table customer (id integer primary key, name text)",
        "insert into customer values (1, 'Ada'), (2, 'Grace')",
        "create table orders (id integer primary key, customer integer, total real)",
        "insert into orders values (10, 1, 9.5)",
    ] {
        run("sqlite", &session, sql).await;
    }
    let folder = scratch("workbook");
    std::fs::create_dir_all(&folder).unwrap();
    let tables = vec![
        ("main".to_string(), "customer".to_string()),
        ("main".to_string(), "orders".to_string()),
    ];
    let files = export_tables(
        &session,
        &tables,
        &folder,
        &FileNamePattern {
            pattern: "{schema}_{table}".into(),
            ..FileNamePattern::default()
        },
        None,
        &xlsx(100),
        &JobCancel::new(),
        &|_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(files.len(), 2);
    let workbook = folder.join("main_tables.xlsx");
    assert!(files.iter().all(|f| f.path == workbook), "{files:?}");
    assert_eq!(files[0].summary.rows, 2);
    assert_eq!(files[1].summary.rows, 1);
    let entries: Vec<_> = std::fs::read_dir(&folder).unwrap().collect();
    assert_eq!(entries.len(), 1, "only the workbook, no part file left");
    let shown = tokio::task::spawn_blocking(move || {
        preview(
            &ImportSource::File(workbook),
            Some(&sheet_options(Some("orders"))),
            100,
            "sqlite",
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(shown.sheets, ["customer", "orders"]);
    assert_eq!(
        shown.rows,
        [vec![
            Some("10".into()),
            Some("1".into()),
            Some("9.5".into())
        ]]
    );
}

#[test]
fn workbooks_are_recognised_and_old_ones_refused() {
    let _turn = crate::serial::file_turn(module_path!());
    let book = scratch("guess.data");
    std::fs::write(&book, b"PK\x03\x04rest").unwrap();
    assert_eq!(
        guess_options(&ImportSource::File(book)).unwrap().format,
        ImportFormat::Xlsx
    );
    let old = scratch("old.xls");
    std::fs::write(&old, b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1").unwrap();
    let error = guess_options(&ImportSource::File(old))
        .unwrap_err()
        .to_string();
    assert!(error.contains(".xls"), "{error}");
    let pasted = preview(
        &ImportSource::Text("a,b\n1,2".into()),
        Some(&sheet_options(None)),
        10,
        "sqlite",
    )
    .unwrap_err();
    assert!(matches!(pasted, ImportError::Invalid(_)), "{pasted:?}");
}

/// SDB-539: a folder an earlier run left under this run's name (Windows reuses process ids) is
/// emptied the first time this run asks for it, so a leftover export cannot push this run's to
/// `name (2).xlsx`; what this run writes there stays for its later calls.
#[test]
fn a_test_folder_left_by_an_earlier_run_is_emptied_first() {
    let _turn = crate::serial::file_turn(module_path!());
    let prefix = "spelaion-xlsx-leftover-check";
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("workbook")).unwrap();
    std::fs::write(
        dir.join("workbook").join("main_tables.xlsx"),
        b"left by an earlier run",
    )
    .unwrap();

    let fresh = crate::support::run_dir(prefix);
    assert_eq!(fresh, dir);
    assert_eq!(std::fs::read_dir(&fresh).unwrap().count(), 0, "emptied");

    std::fs::write(fresh.join("this-run.txt"), b"kept").unwrap();
    let again = crate::support::run_dir(prefix);
    assert!(
        again.join("this-run.txt").is_file(),
        "a later call keeps this run's files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
