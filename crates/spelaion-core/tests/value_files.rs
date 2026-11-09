//! The cell viewer's values on every engine with a binary type: SQLite and DuckDB always,
//! PostgreSQL, MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without
//! them they are skipped, which does not count as tested).
//!
//! An image stored in a binary column is fetched from a table data page on demand, recognised
//! by its bytes and saved to a file byte for byte; a file loaded into a cell becomes exactly
//! one UPDATE by primary key, and reading the page again gives the file back.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    CellContent, CoreEvent, ImageFormat, ResultId, ResultStore, Session, TablePage, VIEW_LIMIT,
    apply_edits, edit_statements, file_change, save_cell, table_page_sql, view_cell,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

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

/// An engine available here: its session, the schema its tables go in, its binary type and
/// the type of its XML column.
struct Engine {
    label: &'static str,
    session: Session,
    schema: &'static str,
    binary: &'static str,
    xml: &'static str,
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
            binary: "BLOB",
            xml: "TEXT",
        },
        Engine {
            label: "duckdb",
            session: session(&spelaion_driver_duckdb::DuckdbDriver::default(), memory()).await,
            schema: "main",
            binary: "BLOB",
            xml: "VARCHAR",
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
            binary: "BYTEA",
            xml: "XML",
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
                binary: "LONGBLOB",
                xml: "LONGTEXT",
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
            binary: "VARBINARY(MAX)",
            xml: "XML",
        });
    }
    out
}

/// Runs each statement on its own; panics on a failure unless `tolerate`. Returns the result
/// of the last statement that produced one (kept in the session's store).
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
                    let shown: String = statement.chars().take(200).collect();
                    panic!("{label}: {shown}\n→ {}", error.message)
                }
                CoreEvent::ResultStarted { result, .. } => last = Some(*result),
                _ => {}
            }
        }
    }
    last
}

const TABLE: &str = "vv_files";

/// The engine's literal for `bytes`.
fn binary_literal(bytes: &[u8], engine: &str) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    match engine {
        "postgres" => format!("'\\x{hex}'"),
        "duckdb" => format!("from_hex('{hex}')"),
        "sqlserver" => format!("0x{hex}"),
        _ => format!("X'{hex}'"),
    }
}

/// Bytes that start with `signature` and go on with `len` pseudo-random bytes: past 64 KB, so
/// a type or a protocol that cuts long values would show.
fn content(signature: &[u8], len: usize, seed: u32) -> Vec<u8> {
    let mut out = signature.to_vec();
    let mut state = seed;
    out.extend((0..len).map(|_| {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (state >> 16) as u8
    }));
    out
}

/// A page of the table ordered by its key, loaded as a table data tab loads it.
async fn page(engine: &Engine) -> ResultId {
    let session = &engine.session;
    let sql = table_page_sql(
        session.dialect().as_ref(),
        &TablePage {
            schema: engine.schema.into(),
            name: TABLE.into(),
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

fn scratch(label: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spelaion-values-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[tokio::test(flavor = "multi_thread")]
async fn images_are_fetched_saved_and_loaded_on_every_engine() {
    let png = content(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", 100_000, 7);
    let gif = content(b"GIF89a", 90_000, 11);
    for engine in engines().await {
        let label = engine.label;
        let session = &engine.session;
        let dialect = session.dialect();
        let lit = |bytes: &[u8]| binary_literal(bytes, dialect.name());
        run(label, session, &[format!("DROP TABLE {TABLE}")], true).await;
        run(
            label,
            session,
            &[
                format!(
                    "CREATE TABLE {TABLE} (id INT PRIMARY KEY, data {}, doc {})",
                    engine.binary, engine.xml
                ),
                format!(
                    "INSERT INTO {TABLE} (id, data, doc) VALUES (1, {}, '<a><b>1</b><c/></a>')",
                    lit(&png)
                ),
                format!(
                    "INSERT INTO {TABLE} (id, data, doc) VALUES (2, {}, 'not xml')",
                    lit(b"\x00\x01plain")
                ),
                format!("INSERT INTO {TABLE} (id, data, doc) VALUES (3, NULL, NULL)"),
            ],
            false,
        )
        .await;

        let result = page(&engine).await;
        let stored = session.results().get(result).expect("result");
        {
            let stored = stored.read().unwrap();
            // The image, on demand: recognised by its bytes and handed over as a data URL.
            let view = view_cell(&stored, 0, 1, false).unwrap();
            assert_eq!(view.image, Some(ImageFormat::Png), "{label}");
            assert!(view.binary, "{label}");
            assert_eq!(view.size, png.len(), "{label}");
            assert_eq!(view.extension, "png", "{label}");
            let CellContent::Bytes { text, image_url } = &view.content else {
                panic!("{label}: {:?}", view.content)
            };
            assert!(text.starts_with("\\x89504e47"), "{label}");
            assert!(
                image_url
                    .as_deref()
                    .is_some_and(|url| url.starts_with("data:image/png;base64,iVBORw0KGgo")),
                "{label}"
            );
            // Saved byte for byte.
            let saved = scratch(label, "saved.png");
            assert_eq!(save_cell(&stored, 0, 1, &saved).unwrap(), png.len() as u64);
            assert_eq!(std::fs::read(&saved).unwrap(), png, "{label}: saved file");
            // Other bytes stay hex, NULL has nothing to save.
            let plain = view_cell(&stored, 1, 1, false).unwrap();
            assert_eq!(plain.image, None, "{label}");
            assert_eq!(plain.extension, "bin", "{label}");
            assert!(
                matches!(&plain.content, CellContent::Bytes { image_url: None, text } if text == "\\x0001706c61696e"),
                "{label}: {:?}",
                plain.content
            );
            assert_eq!(
                view_cell(&stored, 2, 1, false).unwrap().content,
                CellContent::Null
            );
            assert!(save_cell(&stored, 2, 1, &scratch(label, "null.bin")).is_err());
            // XML is pretty-printed; other text is not.
            let CellContent::Text { xml, .. } = view_cell(&stored, 0, 2, false).unwrap().content
            else {
                panic!("{label}: the XML is not text")
            };
            assert_eq!(
                xml.as_deref(),
                Some("<a>\n  <b>1</b>\n  <c/>\n</a>"),
                "{label}"
            );
            assert!(matches!(
                view_cell(&stored, 1, 2, false).unwrap().content,
                CellContent::Text { xml: None, .. }
            ));
        }

        // A file loaded into the second row: one UPDATE by its key, then the file comes back.
        let file = scratch(label, "loaded.gif");
        std::fs::write(&file, &gif).unwrap();
        let table = session
            .connection()
            .describe_table(engine.schema, TABLE)
            .await
            .unwrap_or_else(|e| panic!("{label}: describe: {e}"));
        let statements = {
            let stored = stored.read().unwrap();
            let change = file_change(&stored, 1, 1, &file).unwrap();
            edit_statements(dialect.as_ref(), &table, &stored, &[change]).unwrap()
        };
        assert_eq!(statements.len(), 1, "{label}");
        let id = dialect.quote_identifier("id");
        assert!(statements[0].starts_with("UPDATE "), "{label}");
        assert!(
            statements[0].ends_with(&format!("WHERE {id} = 2")),
            "{label}: {}",
            &statements[0][statements[0].len().saturating_sub(80)..]
        );
        assert_eq!(apply_edits(session, &statements).await, Ok(1), "{label}");
        let again = page(&engine).await;
        let stored = session.results().get(again).expect("result");
        {
            let stored = stored.read().unwrap();
            assert_eq!(
                view_cell(&stored, 1, 1, false).unwrap().image,
                Some(ImageFormat::Gif),
                "{label}"
            );
            let back = scratch(label, "back.gif");
            save_cell(&stored, 1, 1, &back).unwrap();
            assert_eq!(std::fs::read(&back).unwrap(), gif, "{label}: loaded file");
            // The other rows are as they were.
            let first = scratch(label, "first.png");
            save_cell(&stored, 0, 1, &first).unwrap();
            assert_eq!(std::fs::read(&first).unwrap(), png, "{label}: first row");
            assert_eq!(
                view_cell(&stored, 2, 1, false).unwrap().content,
                CellContent::Null
            );
        }
        run(label, session, &[format!("DROP TABLE {TABLE}")], true).await;
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}

/// A value past the limit waits for "load anyway"; it is still recognised and saved whole.
#[tokio::test(flavor = "multi_thread")]
async fn big_values_wait_for_load_anyway() {
    let session = &session(
        &spelaion_driver_sqlite::SqliteDriver::default(),
        ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        },
    )
    .await;
    let big = VIEW_LIMIT + 1;
    let result = run(
        "sqlite",
        session,
        &[format!(
            "SELECT X'89504E470D0A1A0A' || zeroblob({big}) AS data, \
             '<a>' || replace(zeroblob({big}), X'00', 'x') || '</a>' AS doc"
        )],
        false,
    )
    .await
    .expect("result");
    let stored = session.results().get(result).expect("result");
    let stored = stored.read().unwrap();
    let view = view_cell(&stored, 0, 0, false).unwrap();
    assert_eq!(view.content, CellContent::Withheld);
    assert_eq!(view.image, Some(ImageFormat::Png));
    assert_eq!(view.size, big + 8);
    assert!(matches!(
        view_cell(&stored, 0, 0, true).unwrap().content,
        CellContent::Bytes {
            image_url: Some(_),
            ..
        }
    ));
    let text = view_cell(&stored, 0, 1, false).unwrap();
    assert_eq!(text.content, CellContent::Withheld);
    assert!(!text.binary);
    assert!(matches!(
        view_cell(&stored, 0, 1, true).unwrap().content,
        CellContent::Text { xml: Some(_), .. }
    ));
    let saved = scratch("big", "big.bin");
    assert_eq!(save_cell(&stored, 0, 0, &saved).unwrap(), (big + 8) as u64);
    assert_eq!(std::fs::metadata(&saved).unwrap().len(), (big + 8) as u64);
    assert!(view_cell(&stored, 1, 0, false).is_err());
    assert!(view_cell(&stored, 0, 9, false).is_err());
    let _ = std::fs::remove_dir_all(saved.parent().unwrap());
}
