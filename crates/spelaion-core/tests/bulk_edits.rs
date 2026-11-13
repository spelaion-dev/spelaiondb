//! Bulk edits of a table data page, with undo and redo of the pending edits (ADR-044), on every
//! engine: SQLite and DuckDB always, PostgreSQL, MySQL, MariaDB and SQL Server with their
//! `SPELAION_TEST_*` variables (without them they are skipped, which does not count as tested).
//!
//! A page of 1,000 rows is edited in bulk (set a value, trim, upper case, a new UUID, now, a
//! paste, fill down, DEFAULT). Undo and redo must walk back and forth through exactly the
//! edits that were pending; the edits must become 1,000 statements, one per row by primary
//! key; a failure in the last one must roll back the other 999; and once it is fixed, the
//! 1,000 must be applied together.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    Block, BulkAction, CellEdit, CellInput, CoreEvent, EditError, PendingEdit, PendingEdits,
    ResultId, ResultStore, Session, TablePage, apply_edits, edit_statements, table_page_sql,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

const ROWS: usize = 1000;
const TABLE: &str = "bulk_edit_rows";

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

/// Every engine available here, with the schema its table goes in.
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

/// Runs one statement; returns the result it produced, if any.
async fn run(label: &str, session: &Session, sql: &str, tolerate: bool) -> Option<ResultId> {
    let mut result = None;
    let mut events = session.run_script(sql);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::StatementFailed { error, .. } if !tolerate => {
                panic!("{label}: {sql}\n→ {}", error.message)
            }
            CoreEvent::ResultStarted { result: id, .. } => result = Some(id),
            _ => {}
        }
    }
    result
}

/// The first value of a query, as text (NULL as `None`).
async fn scalar(label: &str, session: &Session, sql: &str) -> Option<String> {
    let id = run(label, session, sql, false)
        .await
        .unwrap_or_else(|| panic!("{label}: {sql} returned no rows"));
    let stored = session.results().get(id).unwrap();
    let value = stored.read().unwrap().value_at(0, 0);
    session.results().remove(id);
    match value {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.to_string()),
    }
}

async fn count(label: &str, session: &Session, condition: &str) -> String {
    scalar(
        label,
        session,
        &format!("SELECT COUNT(*) FROM {TABLE} WHERE {condition}"),
    )
    .await
    .unwrap_or_default()
}

/// The table: `label` refuses `BOOM`, the value the last row turns into.
async fn create_table(label: &str, session: &Session) {
    let stamp = match label {
        "sqlserver" => "DATETIME2",
        "mysql" | "mariadb" => "DATETIME",
        _ => "TIMESTAMP",
    };
    run(label, session, &format!("DROP TABLE {TABLE}"), true).await;
    run(
        label,
        session,
        &format!(
            "CREATE TABLE {TABLE} (id INT PRIMARY KEY, \
             label VARCHAR(40) NOT NULL DEFAULT 'none' CHECK (label <> 'BOOM'), \
             note VARCHAR(80), code VARCHAR(36), stamp {stamp})"
        ),
        false,
    )
    .await;
    for chunk in (1..=ROWS).collect::<Vec<_>>().chunks(200) {
        let values: Vec<String> = chunk
            .iter()
            .map(|&id| {
                let text = if id == ROWS {
                    "boom".to_string()
                } else {
                    format!("item {id}")
                };
                format!("({id}, '  {text}  ', '2020-01-01 00:00:00')")
            })
            .collect();
        run(
            label,
            session,
            &format!(
                "INSERT INTO {TABLE} (id, label, stamp) VALUES {}",
                values.join(", ")
            ),
            false,
        )
        .await;
    }
}

async fn load_page(label: &str, session: &Session, schema: &str) -> ResultId {
    let sql = table_page_sql(
        session.dialect().as_ref(),
        &TablePage {
            schema: schema.into(),
            name: TABLE.into(),
            filter: String::new(),
            order_by: session.dialect().quote_identifier("id"),
            limit: ROWS as u64,
            offset: 0,
        },
    )
    .unwrap();
    let id = run(label, session, &sql, false).await.unwrap();
    let rows = session.results().get(id).unwrap().read().unwrap().rows();
    assert_eq!(rows, ROWS, "{label}: page rows");
    id
}

type Shown = (
    Vec<(usize, usize, CellInput)>,
    Vec<usize>,
    Vec<Vec<(usize, CellInput)>>,
);

fn shown(pending: &PendingEdits) -> Shown {
    (
        pending
            .cells()
            .map(|(r, c, input)| (r, c, input.clone()))
            .collect(),
        pending.deleted().collect(),
        pending.inserts().to_vec(),
    )
}

#[tokio::test]
async fn bulk_edits_of_a_thousand_rows_on_every_engine() {
    for (label, session, schema) in engines().await {
        create_table(label, &session).await;
        let page = load_page(label, &session, schema).await;
        let stored = session.results().get(page).unwrap();
        let column = |name: &str| {
            let stored = stored.read().unwrap();
            stored
                .schema()
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
                .unwrap_or_else(|| panic!("{label}: no column {name}"))
        };
        let (label_col, note_col, code_col, stamp_col) = (
            column("label"),
            column("note"),
            column("code"),
            column("stamp"),
        );
        let block = |first_row: usize, last_row: usize, columns: Vec<usize>| Block {
            first_row,
            last_row,
            columns,
        };
        let all = ROWS - 1;
        let edits = vec![
            PendingEdit::Bulk {
                block: block(0, all, vec![note_col]),
                action: BulkAction::Set(CellInput::Text("bulk".into())),
            },
            PendingEdit::Bulk {
                block: block(0, all, vec![label_col]),
                action: BulkAction::Trim,
            },
            PendingEdit::Bulk {
                block: block(0, all, vec![label_col]),
                action: BulkAction::UpperCase,
            },
            PendingEdit::Bulk {
                block: block(0, 9, vec![code_col]),
                action: BulkAction::NewUuid,
            },
            PendingEdit::Bulk {
                block: block(0, 9, vec![stamp_col]),
                action: BulkAction::Now,
            },
            PendingEdit::Paste {
                selection: block(10, 10, vec![note_col]),
                columns: vec![note_col, code_col],
                text: "pasted\tp-1\npasted\tp-2\n".into(),
            },
            PendingEdit::Bulk {
                block: block(11, 20, vec![code_col]),
                action: BulkAction::FillDown,
            },
            PendingEdit::Bulk {
                block: block(0, 0, vec![label_col]),
                action: BulkAction::Set(CellInput::Default),
            },
        ];
        let mut states = Vec::new();
        {
            let mut stored = stored.write().unwrap();
            states.push(shown(stored.pending()));
            for edit in edits {
                let outcome = stored
                    .edit_pending(edit.clone())
                    .unwrap_or_else(|e| panic!("{label}: {edit:?}: {e}"));
                assert!(outcome.changed > 0, "{label}: {edit:?} changed nothing");
                states.push(shown(stored.pending()));
            }
            assert_eq!(
                stored.pending().cells().count(),
                ROWS * 2 + 10 + 2 + 9 + 10,
                "{label}: cells pending"
            );
            // Undo walks back through exactly what was pending, redo forward again.
            for expected in states.iter().rev().skip(1) {
                stored.edit_pending(PendingEdit::Undo).unwrap();
                assert_eq!(&shown(stored.pending()), expected, "{label}: undo");
            }
            assert!(stored.pending().is_empty(), "{label}");
            for expected in states.iter().skip(1) {
                stored.edit_pending(PendingEdit::Redo).unwrap();
                assert_eq!(&shown(stored.pending()), expected, "{label}: redo");
            }
        }

        // One UPDATE per row, by primary key.
        let table = session
            .connection()
            .describe_table(schema, TABLE)
            .await
            .unwrap();
        let edit_target = session.edit_target().await.unwrap();
        let statements = {
            let stored = stored.read().unwrap();
            edit_statements(&edit_target, &table, &stored, &stored.pending().changes()).unwrap()
        };
        assert_eq!(statements.len(), ROWS, "{label}: statements");
        let id = session.dialect().quote_identifier("id");
        for (row, statement) in statements.iter().enumerate() {
            assert!(statement.starts_with("UPDATE "), "{label}: {statement}");
            assert!(
                statement.ends_with(&format!("WHERE {id} = {}", row + 1)),
                "{label}: {statement}"
            );
        }

        // The last row breaks the CHECK: all 1,000 are rolled back.
        let failed = apply_edits(&session, &statements).await;
        assert!(
            matches!(failed, Err(EditError::Failed { index: ROWS, .. })),
            "{label}: {failed:?}"
        );
        assert_eq!(
            count(label, &session, "note IS NOT NULL").await,
            "0",
            "{label}"
        );
        assert_eq!(
            count(label, &session, "label LIKE '  item %'").await,
            (ROWS - 1).to_string(),
            "{label}"
        );
        assert_eq!(
            count(label, &session, "stamp > '2021-01-01 00:00:00'").await,
            "0",
            "{label}"
        );
        // The edits are still pending: fix the last row and apply the 1,000 together.
        {
            let mut stored = stored.write().unwrap();
            assert_eq!(&shown(stored.pending()), states.last().unwrap(), "{label}");
            stored
                .edit_pending(PendingEdit::SetCells(vec![CellEdit {
                    row: all,
                    column: label_col,
                    value: Some(CellInput::Text("fine".into())),
                }]))
                .unwrap();
        }
        let edit_target = session.edit_target().await.unwrap();
        let statements = {
            let stored = stored.read().unwrap();
            edit_statements(&edit_target, &table, &stored, &stored.pending().changes()).unwrap()
        };
        assert_eq!(statements.len(), ROWS, "{label}");
        assert_eq!(
            apply_edits(&session, &statements).await,
            Ok(ROWS),
            "{label}"
        );
        assert_eq!(
            count(label, &session, "note = 'bulk'").await,
            (ROWS - 2).to_string(),
            "{label}"
        );
        assert_eq!(
            count(label, &session, "note = 'pasted'").await,
            "2",
            "{label}"
        );
        assert_eq!(
            count(label, &session, "label LIKE 'ITEM %'").await,
            (ROWS - 2).to_string(),
            "{label}: trimmed and upper case"
        );
        let first = |column: &str| format!("SELECT {column} FROM {TABLE} WHERE id = 1");
        assert_eq!(
            scalar(label, &session, &first("label")).await.as_deref(),
            Some("none"),
            "{label}: DEFAULT"
        );
        let uuid = scalar(label, &session, &first("code")).await.unwrap();
        assert_eq!(uuid.len(), 36, "{label}: {uuid}");
        assert_eq!(
            count(label, &session, "stamp > '2021-01-01 00:00:00'").await,
            "10",
            "{label}: now"
        );
        assert_eq!(
            count(label, &session, "code = 'p-2'").await,
            "10",
            "{label}: pasted, then filled down"
        );
        session.results().remove(page);
        run(label, &session, &format!("DROP TABLE {TABLE}"), true).await;
    }
}
