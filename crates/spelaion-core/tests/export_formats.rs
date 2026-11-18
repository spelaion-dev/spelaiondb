//! The export formats and options of SDB-391 on every engine whose exports stream (ADR-034):
//! SQLite and DuckDB always; PostgreSQL, MySQL, MariaDB and SQL Server with their
//! `SPELAION_TEST_*` variables (without them they are skipped, which does not count as
//! tested).
//!
//! A table holds values that are easy to write wrongly (a quote, a backslash, a line break,
//! accents, a negative decimal, a large float, NULL, bytes, a boolean, a date and a timestamp).
//! It is exported as Parquet, Arrow and NDJSON, and each file is read back (Parquet and Arrow
//! with the arrow crates, NDJSON line by line): every cell must be the one the result store
//! holds for the same SELECT. A SQL export in the engine's upsert form runs twice against the
//! table without an error, and puts back a row changed in between. A masked column is
//! exported as the mask in every format.

mod engines;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, Decimal128Array, RecordBatch};
use arrow::datatypes::DataType;
use futures_util::StreamExt;
use spelaion_config::MaskingRules;
use spelaion_core::{
    CoreEvent, CsvOptions, DEFAULT_BATCH_ROWS, ExportCancel, ExportFormat, JsonOptions, MASK,
    MaskPolicy, ResultId, ResultStore, Session, SqlOptions, StreamExport, TableSource,
    TextEncoding, export_table,
};
use spelaion_driver_api::{ColumnMeta, ConnectParams, Driver, LogicalType, Secret, TlsMode, Value};

const TABLE: &str = "export_formats";

struct Case {
    label: &'static str,
    driver: Arc<dyn Driver>,
    params: ConnectParams,
    schema: &'static str,
    setup: Vec<String>,
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
    let dir = std::env::temp_dir().join(format!("spelaion-export-formats-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// The table with each engine's types and literals: `backslash` is how a string literal holds
/// one backslash, `bytes` the binary literal of `DE AD 00`.
fn setup(types: [&str; 9], backslash: &str, bytes: &str, truth: [&str; 2]) -> Vec<String> {
    let [id, name, amount, ratio, flag, day, at, raw, note] = types;
    let [yes, no] = truth;
    vec![
        format!("drop table if exists {TABLE}"),
        format!(
            "create table {TABLE} (id {id} not null primary key, name {name}, amount {amount}, \
             ratio {ratio}, flag {flag}, day {day}, seen_at {at}, raw {raw}, note {note})"
        ),
        format!(
            "insert into {TABLE} (id, name, amount, ratio, flag, day, seen_at, raw, note) values \
             (1, 'O''Brien {backslash} back', 12.50, 1.5, {yes}, '2024-03-04', \
             '2024-03-04 10:00:00.5', {bytes}, null)"
        ),
        format!(
            "insert into {TABLE} (id, name, amount, ratio, flag, day, seen_at, raw, note) values \
             (2, 'line one\nline two: ñ €', -0.25, -22500000000.25, {no}, '1999-12-31', \
             '1999-12-31 23:59:59', null, 'kept')"
        ),
        format!(
            "insert into {TABLE} (id, name, amount, ratio, flag, day, seen_at, raw, note) values \
             (3, '', 0, 0, null, null, null, null, 'z')"
        ),
    ]
}

async fn cases() -> Vec<Case> {
    let duck = scratch("formats.duckdb");
    engines::create_database(spelaion_config::Engine::Duckdb, &duck).await;
    let sqlite = scratch("formats.sqlite");
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
            setup: setup(
                [
                    "integer", "text", "real", "real", "boolean", "date", "text", "blob", "text",
                ],
                "\\",
                "X'DEAD00'",
                ["true", "false"],
            ),
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
                    "decimal(10,2)",
                    "double",
                    "boolean",
                    "date",
                    "timestamp",
                    "blob",
                    "varchar",
                ],
                "\\",
                "from_hex('DEAD00')",
                ["true", "false"],
            ),
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
                    "numeric(10,2)",
                    "double precision",
                    "boolean",
                    "date",
                    "timestamp(3)",
                    "bytea",
                    "text",
                ],
                "\\",
                "'\\xdead00'",
                ["true", "false"],
            ),
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
                        "varchar(100)",
                        "decimal(10,2)",
                        "double",
                        "boolean",
                        "date",
                        "datetime(3)",
                        "varbinary(16)",
                        "text",
                    ],
                    "\\\\",
                    "X'DEAD00'",
                    ["true", "false"],
                ),
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
                    "nvarchar(100)",
                    "decimal(10,2)",
                    "float",
                    "bit",
                    "date",
                    "datetime2(3)",
                    "varbinary(16)",
                    "nvarchar(max)",
                ],
                "\\",
                "0xDEAD00",
                ["1", "0"],
            ),
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

fn options(format: ExportFormat) -> StreamExport {
    StreamExport {
        format,
        header: true,
        csv: CsvOptions::default(),
        encoding: TextEncoding::Utf8,
        gzip: false,
        sql: SqlOptions::default(),
        json: JsonOptions::default(),
        columns: None,
        // Small batches: Parquet writes a row group per batch, and a file of several must read
        // back whole.
        batch_size: 2,
    }
}

fn table() -> TableSource {
    TableSource {
        schema: String::new(),
        name: TABLE.into(),
        filter: String::new(),
        order_by: "id".into(),
    }
}

/// Exports the table through a session of its own, as the app does (ADR-034).
async fn export(
    case: &Case,
    store: &Arc<ResultStore>,
    masking: Option<MaskPolicy>,
    export: &StreamExport,
    path: &Path,
) {
    let session = open(case, store, masking).await;
    let source = TableSource {
        schema: case.schema.into(),
        ..table()
    };
    export_table(
        &session,
        &source,
        path,
        export,
        &ExportCancel::new(),
        &|_| {},
    )
    .await
    .unwrap_or_else(|e| panic!("{}: {:?}: {e}", case.label, export.format));
}

/// A decimal's text without what says nothing: `12.50` and `12.5` are the same number.
fn plain_decimal(text: &str) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text.to_string()
    }
}

/// A cell of a file's batch as text, read with the logical type its metadata names.
fn cell(batch: &RecordBatch, column: usize, row: usize) -> String {
    let field = batch.schema().field(column).clone();
    let array = batch.column(column);
    if array.is_null(row) {
        return "NULL".into();
    }
    if let DataType::Decimal128(_, _) = field.data_type() {
        let decimals = array.as_any().downcast_ref::<Decimal128Array>().unwrap();
        return plain_decimal(&decimals.value_as_string(row));
    }
    let meta = ColumnMeta::from_field(&field).expect("the core's metadata");
    let value =
        Value::from_array(array.as_ref(), row, meta.logical).expect("the logical type's array");
    if meta.logical == LogicalType::Decimal {
        plain_decimal(&value.to_string())
    } else {
        value.to_string()
    }
}

/// Every cell of the stored result, as [`cell`] reads the file's.
fn expected(session: &Session, id: ResultId) -> Vec<Vec<String>> {
    let stored = session.results().get(id).expect("kept");
    let stored = stored.read().unwrap();
    let metas = stored.schema().columns.clone();
    (0..stored.rows())
        .map(|row| {
            metas
                .iter()
                .enumerate()
                .map(|(c, meta)| match stored.value_at(row, c).unwrap() {
                    Value::Null => "NULL".into(),
                    value if meta.logical == LogicalType::Decimal => {
                        plain_decimal(&value.to_string())
                    }
                    value => value.to_string(),
                })
                .collect()
        })
        .collect()
}

fn read_parquet(path: &Path) -> Vec<RecordBatch> {
    let file = std::fs::File::open(path).unwrap();
    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .unwrap()
        .build()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn read_arrow(path: &Path) -> Vec<RecordBatch> {
    let file = std::fs::File::open(path).unwrap();
    arrow::ipc::reader::FileReader::try_new(file, None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn rows_of(batches: &[RecordBatch]) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for batch in batches {
        for row in 0..batch.num_rows() {
            rows.push(
                (0..batch.num_columns())
                    .map(|c| cell(batch, c, row))
                    .collect(),
            );
        }
    }
    rows
}

#[tokio::test(flavor = "multi_thread")]
async fn every_format_reads_back_on_every_engine() {
    for case in cases().await {
        let label = case.label;
        let store = Arc::new(ResultStore::new());
        let work = open(&case, &store, None).await;
        for sql in &case.setup {
            run(label, &work, sql).await;
        }
        let id = run(label, &work, &format!("select * from {TABLE} order by id"))
            .await
            .expect("a result");
        let want = expected(&work, id);
        assert_eq!(want.len(), 3, "{label}");

        // Parquet and Arrow: typed columns, every cell back, several row groups.
        for (format, name) in [
            (ExportFormat::Parquet, "rows.parquet"),
            (ExportFormat::ArrowIpc, "rows.arrow"),
        ] {
            let path = scratch(&format!("{label}-{name}"));
            export(&case, &store, None, &options(format.clone()), &path).await;
            let batches = if format == ExportFormat::Parquet {
                read_parquet(&path)
            } else {
                read_arrow(&path)
            };
            let groups = if format == ExportFormat::Parquet {
                let file = std::fs::File::open(&path).unwrap();
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
                    .unwrap()
                    .metadata()
                    .num_row_groups()
            } else {
                batches.len()
            };
            // A row group (an Arrow batch) per batch of 2 rows: nothing held them all.
            assert_eq!(groups, 2, "{label} {name}");
            assert_eq!(rows_of(&batches), want, "{label} {name}");
            let schema = batches[0].schema();
            let id_type = schema.field(0).data_type();
            assert!(
                matches!(id_type, DataType::Int64),
                "{label} {name}: id is {id_type:?}"
            );
            let raw_type = schema.field(7).data_type();
            assert_eq!(raw_type, &DataType::Binary, "{label} {name}");
            // A decimal with a declared precision is a decimal (SQLite's values say no type).
            if label != "sqlite" {
                let amount = schema.field(2).data_type();
                assert!(
                    matches!(amount, DataType::Decimal128(10, 2) | DataType::Utf8),
                    "{label} {name}: amount is {amount:?}"
                );
            }
        }

        // NDJSON, line by line.
        let path = scratch(&format!("{label}-rows.ndjson"));
        export(&case, &store, None, &options(ExportFormat::Ndjson), &path).await;
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{label}: {text}");
        for (line, row) in lines.iter().zip(&want) {
            let object: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{label}: {line}: {e}"));
            assert_eq!(object["id"].to_string(), row[0], "{label}");
            match object["name"].as_str() {
                Some(name) => assert_eq!(name, row[1], "{label}"),
                None => assert_eq!("NULL", row[1], "{label}"),
            }
        }

        // An upsert, run twice: no error, the same rows, and a changed row put back.
        let path = scratch(&format!("{label}-upsert.sql"));
        let mut upsert = options(ExportFormat::SqlInsert {
            table: TABLE.into(),
        });
        upsert.sql = SqlOptions {
            upsert: true,
            rows_per_insert: 2,
            ..SqlOptions::default()
        };
        export(&case, &store, None, &upsert, &path).await;
        let script = std::fs::read_to_string(&path).unwrap();
        run(label, &work, &script).await;
        run(
            label,
            &work,
            &format!("update {TABLE} set name = 'changed', note = 'changed' where id = 2"),
        )
        .await;
        run(label, &work, &script).await;
        let back = run(label, &work, &format!("select * from {TABLE} order by id"))
            .await
            .expect("a result");
        assert_eq!(expected(&work, back), want, "{label}: upsert\n{script}");

        // A masked column is the mask in every format; NULL stays NULL.
        let masking = MaskPolicy::new(&MaskingRules {
            required: vec![],
            exemptible: vec!["note".into()],
            never: vec![],
        });
        for (format, name) in [
            (ExportFormat::Parquet, "masked.parquet"),
            (ExportFormat::ArrowIpc, "masked.arrow"),
            (ExportFormat::Ndjson, "masked.ndjson"),
        ] {
            let path = scratch(&format!("{label}-{name}"));
            export(
                &case,
                &store,
                Some(masking.clone()),
                &options(format.clone()),
                &path,
            )
            .await;
            let notes: Vec<String> = match format {
                ExportFormat::Ndjson => std::fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .map(|line| {
                        let object: serde_json::Value = serde_json::from_str(line).unwrap();
                        object["note"].as_str().unwrap_or("NULL").to_string()
                    })
                    .collect(),
                _ => {
                    let batches = if format == ExportFormat::Parquet {
                        read_parquet(&path)
                    } else {
                        read_arrow(&path)
                    };
                    rows_of(&batches)
                        .into_iter()
                        .map(|row| row[8].clone())
                        .collect()
                }
            };
            assert_eq!(
                notes,
                vec!["NULL".to_string(), MASK.into(), MASK.into()],
                "{label} {name}"
            );
            let bytes = std::fs::read(&path).unwrap();
            for secret in ["kept", "\"z\""] {
                assert!(
                    !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                    "{label} {name}: {secret} is in the file"
                );
            }
        }

        run(label, &work, &format!("drop table {TABLE}")).await;
    }
}

/// An upsert needs a table with a primary key, and the whole key in the export.
#[tokio::test(flavor = "multi_thread")]
async fn upserts_without_a_key_are_refused() {
    let path = scratch("nokey.sqlite");
    std::fs::write(&path, b"").unwrap();
    let case = Case {
        label: "sqlite",
        driver: engines::sqlite(),
        params: ConnectParams {
            database: path.to_string_lossy().into_owned(),
            ..Default::default()
        },
        schema: "main",
        setup: vec![],
    };
    let store = Arc::new(ResultStore::new());
    let work = open(&case, &store, None).await;
    run("sqlite", &work, "create table nokey (a int, b int)").await;
    run(
        "sqlite",
        &work,
        "create table keyed (a int primary key, b int)",
    )
    .await;
    let mut upsert = options(ExportFormat::SqlInsert { table: "x".into() });
    upsert.sql.upsert = true;
    let attempt = |name: &'static str, export: StreamExport| {
        let store = store.clone();
        let case = &case;
        async move {
            let session = open(case, &store, None).await;
            let source = TableSource {
                schema: "main".into(),
                name: name.into(),
                filter: String::new(),
                order_by: String::new(),
            };
            let out = scratch(&format!("{name}.sql"));
            let result = export_table(
                &session,
                &source,
                &out,
                &export,
                &ExportCancel::new(),
                &|_| {},
            )
            .await;
            (result.map(|_| ()).map_err(|e| e.to_string()), out)
        }
    };
    let (result, out) = attempt("nokey", upsert.clone()).await;
    assert!(result.unwrap_err().contains("no primary key"));
    assert!(!out.exists());
    // The key column left out of the export.
    let mut without_key = upsert.clone();
    without_key.columns = Some(vec![1]);
    run("sqlite", &work, "insert into keyed values (1, 2)").await;
    let (result, out) = attempt("keyed", without_key).await;
    assert!(result.unwrap_err().contains("primary key column a"));
    assert!(!out.exists());
    // An upsert written as anything but SQL.
    let mut csv = options(ExportFormat::Csv);
    csv.sql.upsert = true;
    let (result, _) = attempt("keyed", csv).await;
    assert!(result.is_err());
    let _ = DEFAULT_BATCH_ROWS;
}
