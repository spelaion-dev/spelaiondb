//! Column masking (ADR-051) against every engine: SQLite and DuckDB always, PostgreSQL, MySQL,
//! MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without them they are
//! skipped, which does not count as tested).
//!
//! Each test tries to get a masked value out of the core by another path: the grid's windows,
//! copies and exports from the store, streaming exports, sorting, the text filter, column
//! filters, Ctrl+F, the calc panel, the cell viewer, column statistics, the data search, agents'
//! reads, edits that would write the mask back, filtering by a masked cell and following a
//! masked key. Only revealing one cell gives the value.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::MaskingRules;
use spelaion_core::masking::is_masked_field;
use spelaion_core::{
    Block, BulkAction, CellBlock, CellContent, CellEdit, CellInput, ColumnFilter,
    ColumnStatsRequest, CoreEvent, CsvOptions, DataSearchOptions, DataSearchRequest, EditError,
    ExportCancel, ExportFormat, ExportOptions, FilterOp, FindQuery, FollowError, KeyDirection,
    MASK, MaskPolicy, PendingEdit, ReadLimits, ResultId, ResultStore, RevealError, RowChange,
    SearchTable, Session, SortKey, StoredResult, StreamExport, TableKey, TableStatus, TextEncoding,
    ViewError, cell_filter_value, column_in_list, column_stats, data_search, edit_statements,
    export_query, follow, view_cell, write_result,
};
use spelaion_driver_api::{ConnectParams, Driver, LogicalType, Secret, TlsMode, Value};

/// The rules every test uses: `salary` required (as a connection type's), `*email*` and
/// `card_*` exemptible, and `email_verified` left visible.
fn policy() -> MaskPolicy {
    let v = |items: &[&str]| items.iter().map(|s| s.to_string()).collect();
    MaskPolicy::new(&MaskingRules {
        required: v(&["salary"]),
        exemptible: v(&["*email*", "card_*"]),
        never: v(&["email_verified", "salary"]),
    })
}

/// Values that must never come out.
const SECRETS: [&str; 5] = [
    "ada@example.com",
    "linus@example.com",
    "4111111111111111",
    "5500000000000004",
    "7000",
];

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
    .with_masking(policy())
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

/// Every engine available here, with the schema its tables go in, and how to open another
/// session on the same database (for the streaming export).
async fn engines() -> Vec<(&'static str, Session, &'static str, Option<ConnectParams>)> {
    let memory = || ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let mut out = vec![
        (
            "sqlite",
            session(&spelaion_driver_sqlite::SqliteDriver::default(), memory()).await,
            "main",
            None,
        ),
        (
            "duckdb",
            session(&spelaion_driver_duckdb::DuckdbDriver::default(), memory()).await,
            "main",
            None,
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
            session(&spelaion_driver_postgres::PostgresDriver, params.clone()).await,
            "public",
            Some(params),
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
            out.push((
                label,
                session(&driver, params.clone()).await,
                "spelaion",
                Some(params),
            ));
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push((
            "sqlserver",
            session(&spelaion_driver_sqlserver::SqlServerDriver, params.clone()).await,
            "dbo",
            Some(params),
        ));
    }
    out
}

/// Runs a script; returns the last result it stored. Panics on a failure unless `tolerate`.
async fn run(label: &str, session: &Session, sql: &str, tolerate: bool) -> Option<ResultId> {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let mut last = None;
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } if !tolerate => {
                panic!("{label}: {sql}: {}", error.message)
            }
            CoreEvent::ResultStarted { result, .. } => last = Some(*result),
            _ => {}
        }
    }
    last
}

/// The tests share these tables on the database servers: they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup(label: &str, session: &Session) {
    for sql in [
        "DROP TABLE IF EXISTS mask_orders",
        "DROP TABLE IF EXISTS mask_accounts",
        "DROP TABLE IF EXISTS mask_customers",
    ] {
        run(label, session, sql, true).await;
    }
    for sql in [
        "CREATE TABLE mask_customers (id INT PRIMARY KEY, name VARCHAR(50), email VARCHAR(100), email_verified INT, card_number VARCHAR(30), salary INT)",
        "INSERT INTO mask_customers VALUES (1, 'Ada', 'ada@example.com', 1, '4111111111111111', 5000), (2, 'Grace', NULL, 0, '5500000000000004', 7000), (3, 'Linus', 'linus@example.com', 1, NULL, 6000)",
        "CREATE TABLE mask_accounts (email VARCHAR(100) PRIMARY KEY, tier VARCHAR(20))",
        "INSERT INTO mask_accounts VALUES ('ada@example.com', 'pro'), ('linus@example.com', 'free')",
        "CREATE TABLE mask_orders (id INT PRIMARY KEY, customer_email VARCHAR(100))",
        "INSERT INTO mask_orders VALUES (10, 'ada@example.com')",
    ] {
        run(label, session, sql, false).await;
    }
}

fn stored(session: &Session, id: ResultId) -> Arc<std::sync::RwLock<StoredResult>> {
    session.results().get(id).expect("result kept")
}

fn column(result: &StoredResult, name: &str) -> usize {
    result
        .schema()
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))
        .unwrap_or_else(|| panic!("no column {name}"))
}

fn assert_no_secret(label: &str, what: &str, text: &str) {
    for secret in SECRETS {
        assert!(
            !text.contains(secret),
            "{label}: {what} leaked {secret}: {text}"
        );
    }
}

/// Every cell of the result as the grid gets it.
fn grid_text(result: &StoredResult) -> String {
    let mut out = String::new();
    for batch in result.window(0, result.rows()) {
        for row in 0..batch.num_rows() {
            for (c, meta) in result.schema().columns.iter().enumerate() {
                let value = Value::from_array(batch.column(c).as_ref(), row, meta.logical)
                    .unwrap_or(Value::Null);
                out.push_str(&value.to_string());
                out.push('|');
            }
            out.push('\n');
        }
    }
    out
}

fn export(result: &StoredResult, format: ExportFormat) -> String {
    let mut out = Vec::new();
    write_result(
        result,
        &ExportOptions {
            format,
            rows: 0..result.rows(),
            columns: (0..result.schema().columns.len()).collect(),
            header: true,
            csv: CsvOptions::default(),
        },
        &spelaion_core::GenericDialect,
        &mut out,
    )
    .unwrap();
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn the_grid_copies_and_exports_from_the_store_get_the_mask() {
    let _serial = SERIAL.lock().await;
    for (label, session, _, _) in engines().await {
        setup(label, &session).await;
        let id = run(
            label,
            &session,
            "SELECT id, name, email, email_verified, card_number, salary, email AS contact_email FROM mask_customers ORDER BY id",
            false,
        )
        .await
        .unwrap();
        let result = stored(&session, id);
        let result = result.read().unwrap();
        // The schema says which columns are masked; the others keep their type.
        let masked: Vec<bool> = result
            .schema()
            .arrow
            .fields()
            .iter()
            .map(|f| is_masked_field(f))
            .collect();
        assert_eq!(
            masked,
            [false, false, true, false, true, true, true],
            "{label}"
        );
        let names: Vec<String> = result
            .masked_columns()
            .iter()
            .map(|n| n.to_lowercase())
            .collect();
        assert_eq!(
            names,
            ["email", "card_number", "salary", "contact_email"],
            "{label}"
        );
        let grid = grid_text(&result);
        assert_no_secret(label, "the grid", &grid);
        assert!(
            grid.contains("Ada"),
            "{label}: unmasked columns stay: {grid}"
        );
        // NULL stays NULL; every other masked value is the mask.
        let email = column(&result, "email");
        assert_eq!(result.value_at(1, email), Some(Value::Null), "{label}");
        assert_eq!(
            result.value_at(0, email),
            Some(Value::Text(MASK.into())),
            "{label}"
        );
        assert_eq!(
            result.value_at(0, column(&result, "email_verified")),
            Some(Value::Int(1)),
            "{label}: `never` leaves it visible"
        );
        // Copies and exports written from the store.
        for format in [
            ExportFormat::Csv,
            ExportFormat::Tsv,
            ExportFormat::Json,
            ExportFormat::Markdown,
            ExportFormat::Html,
            ExportFormat::InList,
            ExportFormat::SqlInsert { table: "t".into() },
            ExportFormat::SqlInsertBatch { table: "t".into() },
        ] {
            let text = export(&result, format.clone());
            assert_no_secret(label, &format!("{format:?}"), &text);
            assert!(text.contains(MASK), "{label} {format:?}: {text}");
        }
        let list = column_in_list(&result, email, &spelaion_core::GenericDialect).unwrap();
        assert_no_secret(label, "the IN list", &list);
        // The cell viewer.
        let view = view_cell(&result, 0, email, true).unwrap();
        assert_eq!(
            view.content,
            CellContent::Text {
                text: MASK.into(),
                xml: None
            },
            "{label}"
        );
        // Revealing is the one way to the value, and only for a masked cell.
        let revealed = result.reveal(0, email).unwrap();
        assert_eq!(
            revealed.value,
            Value::Text("ada@example.com".into()),
            "{label}"
        );
        assert_eq!(revealed.column.to_lowercase(), "email");
        let salary = result.reveal(1, column(&result, "salary")).unwrap();
        assert_eq!(salary.value, Value::Int(7000), "{label}");
        assert_eq!(salary.logical, LogicalType::Int, "{label}");
        assert_eq!(result.reveal(1, email).unwrap().value, Value::Null);
        assert_eq!(
            result.reveal(0, column(&result, "name")),
            Err(RevealError::NotMasked)
        );
        assert_eq!(result.reveal(99, email), Err(RevealError::NoSuchCell));
        // Revealing does not unmask anything else.
        assert_no_secret(label, "the grid after revealing", &grid_text(&result));
    }
}

#[tokio::test]
async fn sorting_filtering_searching_and_the_calc_panel_never_see_the_value() {
    let _serial = SERIAL.lock().await;
    for (label, session, _, _) in engines().await {
        setup(label, &session).await;
        let id = run(
            label,
            &session,
            "SELECT id, email, salary FROM mask_customers ORDER BY id",
            false,
        )
        .await
        .unwrap();
        let result = stored(&session, id);
        let mut result = result.write().unwrap();
        let email = column(&result, "email");
        let salary = column(&result, "salary");
        // Sorting by a masked column is refused: its order would tell the values apart.
        for column in [email, salary] {
            for descending in [false, true] {
                assert!(
                    matches!(
                        result.sort(Some(SortKey { column, descending })),
                        Err(ViewError::Masked(_))
                    ),
                    "{label}"
                );
            }
        }
        assert_eq!(result.sort_key(), None);
        // The text filter matches the mask, never the value.
        assert_eq!(result.filter(Some("ada@example")).unwrap(), 0, "{label}");
        assert_eq!(result.filter(Some("7000")).unwrap(), 0, "{label}");
        assert_eq!(result.filter(Some(MASK)).unwrap(), 3, "{label}");
        result.filter(None).unwrap();
        // Column filters too.
        for (column, op, value) in [
            (email, FilterOp::Eq, "ada@example.com"),
            (email, FilterOp::Contains, "@"),
            (email, FilterOp::Like, "%example%"),
            (salary, FilterOp::Contains, "7000"),
        ] {
            let rows = result
                .filter_columns(vec![ColumnFilter {
                    column,
                    op,
                    value: value.into(),
                }])
                .unwrap();
            assert_eq!(rows, 0, "{label}: {op:?} {value}");
        }
        assert_eq!(
            result
                .filter_columns(vec![ColumnFilter {
                    column: email,
                    op: FilterOp::IsNull,
                    value: String::new(),
                }])
                .unwrap(),
            1,
            "{label}: NULL stays NULL"
        );
        result.filter_columns(Vec::new()).unwrap();
        // Ctrl+F.
        for text in ["ada@example.com", "example", "7000"] {
            let found = result
                .find_matches(
                    &FindQuery {
                        text: text.into(),
                        match_case: false,
                        columns: vec![email, salary],
                    },
                    &result.find_ticket(),
                )
                .unwrap()
                .unwrap();
            assert_eq!(found.len(), 0, "{label}: Ctrl+F found {text}");
        }
        // The calc panel.
        let stats = result
            .selection_stats(
                &CellBlock {
                    first_row: 0,
                    rows: 3,
                    columns: vec![email, salary],
                },
                &result.stats_ticket(),
            )
            .unwrap()
            .unwrap();
        let shown = format!("{stats:?}");
        assert_no_secret(label, "the calc panel", &shown);
        assert_eq!(stats.numbers, 0, "{label}: a masked number is not a number");
        assert_eq!(stats.sum, None, "{label}");
        assert_eq!(stats.distinct, 1, "{label}");
    }
}

#[tokio::test]
async fn edits_never_write_the_mask_or_read_through_it() {
    let _serial = SERIAL.lock().await;
    for (label, session, schema, _) in engines().await {
        setup(label, &session).await;
        let id = run(
            label,
            &session,
            "SELECT id, name, email, card_number FROM mask_customers ORDER BY id",
            false,
        )
        .await
        .unwrap();
        // The catalog is read before the result is locked: no lock is held across an await.
        let table = session
            .connection()
            .describe_table(schema, "mask_customers")
            .await
            .unwrap();
        let accounts_table = session
            .connection()
            .describe_table(schema, "mask_accounts")
            .await
            .unwrap();
        let accounts_id = run(
            label,
            &session,
            "SELECT email, tier FROM mask_accounts ORDER BY tier",
            false,
        )
        .await
        .unwrap();
        let edit_target = session.edit_target().await.unwrap();
        let result = stored(&session, id);
        let mut result = result.write().unwrap();
        let email = column(&result, "email");
        let name = column(&result, "name");
        let text = |t: &str| Some(CellInput::Text(t.into()));
        let refused = |outcome: Result<spelaion_core::EditOutcome, EditError>| {
            matches!(outcome, Err(EditError::Masked(_)))
        };
        // Typing into a masked cell, or setting it.
        assert!(refused(result.edit_pending(PendingEdit::Type(CellEdit {
            row: 0,
            column: email,
            value: text("x"),
        }))));
        assert!(refused(result.edit_pending(PendingEdit::SetCells(vec![
            CellEdit {
                row: 0,
                column: name,
                value: text("Ada L."),
            },
            CellEdit {
                row: 0,
                column: email,
                value: Some(CellInput::Null),
            },
        ]))));
        // Bulk edits that would compute from the mask, or overwrite it.
        let block = Block {
            first_row: 0,
            last_row: 2,
            columns: vec![name, email],
        };
        for action in [
            BulkAction::FillDown,
            BulkAction::Trim,
            BulkAction::UpperCase,
            BulkAction::LowerCase,
            BulkAction::Set(CellInput::Null),
        ] {
            assert!(
                refused(result.edit_pending(PendingEdit::Bulk {
                    block: block.clone(),
                    action: action.clone(),
                })),
                "{label}: {action:?}"
            );
        }
        // A paste that reaches a masked column.
        assert!(refused(result.edit_pending(PendingEdit::Paste {
            selection: Block {
                first_row: 0,
                last_row: 0,
                columns: vec![name],
            },
            columns: vec![name, email],
            text: "A\tB".into(),
        })));
        // A duplicated row would carry the mask as its value.
        assert!(refused(result.edit_pending(PendingEdit::Insert(vec![
            (0, CellInput::Text("4".into())),
            (email, CellInput::Text(MASK.into())),
        ]))));
        // Nothing of the above is pending; an unmasked edit and a typed new row still work.
        assert!(result.pending().is_empty(), "{label}");
        result
            .edit_pending(PendingEdit::Type(CellEdit {
                row: 0,
                column: name,
                value: text("Ada L."),
            }))
            .unwrap();
        result
            .edit_pending(PendingEdit::Insert(vec![
                (0, CellInput::Text("4".into())),
                (email, CellInput::Text("new@example.org".into())),
            ]))
            .unwrap();
        // The statements: updating a masked column is refused even if asked directly.
        assert!(matches!(
            edit_statements(
                &edit_target,
                &table,
                &result,
                &[RowChange::Update {
                    row: 0,
                    values: vec![(email, CellInput::Text("x@example.org".into()))],
                }],
            ),
            Err(EditError::Masked(_))
        ));
        drop(result);

        // A table whose primary key is masked: its rows cannot be updated or deleted, since
        // the statement would need the value.
        let accounts = stored(&session, accounts_id);
        let accounts = accounts.read().unwrap();
        let table = accounts_table;
        let tier = column(&accounts, "tier");
        for change in [
            RowChange::Update {
                row: 0,
                values: vec![(tier, CellInput::Text("team".into()))],
            },
            RowChange::Delete { row: 0 },
        ] {
            assert!(
                matches!(
                    edit_statements(&edit_target, &table, &accounts, &[change]),
                    Err(EditError::Masked(_))
                ),
                "{label}"
            );
        }
        // Filtering by a masked cell, and following a key through a masked column.
        assert!(cell_filter_value(&accounts, 0, column(&accounts, "email")).is_err());
        assert_eq!(
            cell_filter_value(&accounts, 0, tier).unwrap(),
            Value::Text("free".into())
        );
        let key = TableKey {
            direction: KeyDirection::Incoming,
            name: "fk_orders".into(),
            schema: schema.into(),
            table: "mask_orders".into(),
            columns: vec!["email".into()],
            other_columns: vec!["customer_email".into()],
        };
        assert!(
            matches!(
                follow(session.dialect().as_ref(), &accounts, 0, &key),
                Err(FollowError::Masked(_))
            ),
            "{label}"
        );
    }
}

#[tokio::test]
async fn streaming_exports_statistics_search_and_agents_get_the_mask() {
    let _serial = SERIAL.lock().await;
    let dir = std::env::temp_dir().join(format!("spelaion-masking-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (label, session, schema, _) in engines().await {
        setup(label, &session).await;
        // A streaming export (ADR-034) masks every batch it writes.
        for format in [ExportFormat::Csv, ExportFormat::Json] {
            let path =
                dir.join(format!("{label}-{format:?}.txt").replace(['{', '}', ' ', ':'], ""));
            export_query(
                &session,
                "SELECT id, email, card_number, salary FROM mask_customers ORDER BY id",
                &Default::default(),
                false,
                &path,
                &StreamExport {
                    format: format.clone(),
                    header: true,
                    csv: CsvOptions::default(),
                    encoding: TextEncoding::Utf8,
                    columns: None,
                    batch_size: 1,
                },
                &ExportCancel::new(),
                &|_| {},
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
            let text = std::fs::read_to_string(&path).unwrap();
            assert_no_secret(label, "a streaming export", &text);
            assert!(text.contains(MASK), "{label}: {text}");
            let _ = std::fs::remove_file(&path);
        }

        // Column statistics: rows, NULLs and distinct are counted, values are the mask, and no
        // frequent value carries a condition that would hold it.
        for (name, logical) in [("email", LogicalType::Text), ("salary", LogicalType::Int)] {
            let stats = column_stats(
                &session,
                &ColumnStatsRequest {
                    schema: schema.into(),
                    table: "mask_customers".into(),
                    column: name.into(),
                    logical,
                    filter: String::new(),
                    sample_rows: 100_000,
                    top: 10,
                },
                &ExportCancel::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(stats.masked, "{label}");
            assert_eq!(stats.rows, 3, "{label}");
            let shown = format!("{:?} {:?}", stats.range, stats.top);
            assert_no_secret(label, "the statistics", &shown);
            assert!(shown.contains(MASK), "{label}: {shown}");
            if let spelaion_core::Stat::Value(top) = &stats.top {
                for value in top {
                    assert!(
                        value.text.is_none() || value.condition.is_none(),
                        "{label}: {value:?}"
                    );
                }
            }
        }

        // The data search finds rows by a masked column's value (the user typed it) but shows
        // the mask, and writes no filter from a masked key.
        if session.connection().capabilities().bound_params {
            let found = std::sync::Mutex::new(Vec::new());
            data_search(
                &session,
                &DataSearchRequest {
                    value: "ada@example".into(),
                    tables: vec![
                        SearchTable {
                            schema: schema.into(),
                            table: "mask_customers".into(),
                        },
                        SearchTable {
                            schema: schema.into(),
                            table: "mask_accounts".into(),
                        },
                    ],
                    options: DataSearchOptions::default(),
                },
                &ExportCancel::new(),
                &|event| {
                    if let spelaion_core::DataSearchEvent::Finished(outcome) = event {
                        found.lock().unwrap().push(format!("{outcome:?}"));
                    }
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
            let text = found.into_inner().unwrap().join("\n");
            assert_no_secret(label, "the data search", &text);
            assert!(text.contains(MASK), "{label}: {text}");
            assert!(!text.contains("Failed"), "{label}: {text}");
        }

        // Agents' reads: the rows `run_read_only` returns are masked, and say which columns.
        let read = session
            .run_read_only(
                "SELECT id, email, card_number, salary, email_verified FROM mask_customers ORDER BY id",
                &ReadLimits::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(read.masked, vec![1, 2, 3], "{label}");
        assert_no_secret(label, "an agent's read", &format!("{:?}", read.rows));
        assert_eq!(read.rows[1][1], Value::Null, "{label}");
        assert_eq!(read.rows[0][1], Value::Text(MASK.into()), "{label}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_session_without_rules_masks_nothing() {
    let driver = spelaion_driver_sqlite::SqliteDriver::default();
    let conn = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    setup("sqlite", &session).await;
    let id = run(
        "sqlite",
        &session,
        "SELECT email FROM mask_customers",
        false,
    )
    .await
    .unwrap();
    let result = stored(&session, id);
    let result = result.read().unwrap();
    assert!(grid_text(&result).contains("ada@example.com"));
    assert!(result.masked_columns().is_empty());
    assert_eq!(result.reveal(0, 0), Err(RevealError::NotMasked));
    let _ = TableStatus::NoColumns;
}

/// ADR-052: where the engine says which table column a result column reads, an alias does not
/// unmask it, and renaming a masked column to an exempted name does not either. Where it does
/// not (SQL Server, DuckDB), only result names count: a known limit the test pins down, so the
/// day an engine reports origins this test says so.
#[tokio::test]
async fn an_alias_does_not_unmask_a_column_where_the_engine_reports_its_origin() {
    let _serial = SERIAL.lock().await;
    let dir = std::env::temp_dir().join(format!("spelaion-masking-alias-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sql = "SELECT id, email AS e, salary AS pay, card_number AS c, email_verified AS email, email AS email_verified, name AS n FROM mask_customers ORDER BY id";
    for (label, session, _, _) in engines().await {
        setup(label, &session).await;
        let reports_origin = matches!(label, "sqlite" | "postgres" | "mysql" | "mariadb");
        let id = run(label, &session, sql, false).await.unwrap();
        {
            let result = stored(&session, id);
            let result = result.read().unwrap();
            let names: Vec<String> = result
                .masked_columns()
                .iter()
                .map(|n| n.to_lowercase())
                .collect();
            if reports_origin {
                // `email` here is email_verified's value (exempt); `email_verified` is email's.
                assert_eq!(names, ["e", "pay", "c", "email_verified"], "{label}");
                assert_no_secret(label, "the grid", &grid_text(&result));
                for format in [ExportFormat::Csv, ExportFormat::Json, ExportFormat::InList] {
                    let text = export(&result, format.clone());
                    assert_no_secret(label, &format!("{format:?}"), &text);
                }
                let schema = result.schema();
                let e = schema
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case("e"))
                    .unwrap();
                let origin = e.origin.as_ref().expect("the origin of `e`");
                assert!(
                    origin.table.eq_ignore_ascii_case("mask_customers"),
                    "{label}"
                );
                assert!(origin.column.eq_ignore_ascii_case("email"), "{label}");
            } else {
                // Known limit (ADR-052): no origin, so only the result name `email` matches.
                assert_eq!(names, ["email"], "{label}");
                assert!(result.schema().columns.iter().all(|c| c.origin.is_none()));
            }
        }
        if !reports_origin {
            continue;
        }
        // A streaming export of the same statement.
        let path = dir.join(format!("{label}.csv"));
        export_query(
            &session,
            sql,
            &Default::default(),
            false,
            &path,
            &StreamExport {
                format: ExportFormat::Csv,
                header: true,
                csv: CsvOptions::default(),
                encoding: TextEncoding::Utf8,
                columns: None,
                batch_size: 1,
            },
            &ExportCancel::new(),
            &|_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_no_secret(label, "a streaming export", &text);
        assert!(text.contains(MASK), "{label}: {text}");
        // An agent's read (the MCP's `run_query`).
        let read = session
            .run_read_only(sql, &ReadLimits::default())
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(read.masked, vec![1, 2, 3, 5], "{label}");
        assert_no_secret(label, "an agent's read", &format!("{:?}", read.rows));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
