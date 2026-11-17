//! How the grid shows values (SDB-385) against every engine: SQLite and DuckDB always,
//! PostgreSQL, MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without
//! them they are skipped, which does not count as tested).
//!
//! Each engine's date, time, timestamp, timestamp with a zone and decimal types are read
//! from a table and served as a window with a display format: the window carries each cell
//! as the database prints it and, next to it, as it is shown. Then "filter by this value" is
//! written from each formatted cell (the core writes it from the stored value, never from
//! the shown text) and must still keep exactly that row. A masked column is never formatted.

mod engines;

use std::io::Cursor;
use std::sync::Arc;

use arrow::array::{Array, StringArray};
use arrow::ipc::reader::StreamReader;
use futures_util::StreamExt;
use spelaion_config::MaskingRules;
use spelaion_core::{
    BinaryDisplay, CoreEvent, DecimalMark, Display, DisplayFormat, FilterOp, Grouping, KIND_KEY,
    MASK, MaskPolicy, ResultId, ResultStore, Session, ZONE_KEY, add_condition, count_table_rows,
    displayed_text, value_condition, window_ipc,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

async fn session(driver: &dyn Driver, params: ConnectParams, masked: &[&str]) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    let session = Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    if masked.is_empty() {
        return session;
    }
    session.with_masking(MaskPolicy::new(&MaskingRules {
        required: masked.iter().map(|s| s.to_string()).collect(),
        exemptible: Vec::new(),
        never: Vec::new(),
    }))
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

/// Every engine available here, with the schema its tables go in. `masked` are the columns
/// the session masks.
async fn engines(masked: &[&str]) -> Vec<(&'static str, Session, &'static str)> {
    let memory = || ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let mut out = vec![
        (
            "sqlite",
            session(&*engines::sqlite(), memory(), masked).await,
            "main",
        ),
        (
            "duckdb",
            session(&*engines::duckdb(), memory(), masked).await,
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
            session(&*engines::postgres(), params, masked).await,
            "public",
        ));
    }
    for (label, var, driver) in [
        ("mysql", "SPELAION_TEST_MYSQL", engines::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", engines::mariadb()),
    ] {
        if let Some((host, port)) = server(var) {
            let params = login(host, port, "spelaion", "spelaion", "spelaion");
            out.push((label, session(&driver, params, masked).await, "spelaion"));
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push((
            "sqlserver",
            session(&*engines::sqlserver(), params, masked).await,
            "dbo",
        ));
    }
    out
}

async fn run(
    label: &str,
    session: &Session,
    statements: &[&str],
    tolerate: bool,
) -> Option<ResultId> {
    let mut last = None;
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            match event {
                CoreEvent::StatementFailed { error, .. } if !tolerate => {
                    panic!("{label}: {statement}\n→ {}", error.message)
                }
                CoreEvent::ResultStarted { result, .. } => last = Some(*result),
                _ => {}
            }
        }
    }
    last
}

/// What each engine calls its types, and how a point in time with a zone is written there.
/// `None` where the engine has no such type (it is then not in the table).
struct Types {
    date: &'static str,
    time: Option<&'static str>,
    timestamp: &'static str,
    /// The type and the literal of 2026-11-16 20:07:16 UTC written at -05:00.
    zoned: Option<(&'static str, &'static str)>,
    decimal: &'static str,
}

fn types(label: &str) -> Types {
    match label {
        "postgres" => Types {
            date: "date",
            time: Some("time"),
            timestamp: "timestamp",
            zoned: Some(("timestamptz", "'2026-11-16 15:07:16-05'")),
            decimal: "numeric(12, 2)",
        },
        "duckdb" => Types {
            date: "date",
            time: Some("time"),
            timestamp: "timestamp",
            zoned: Some(("timestamptz", "'2026-11-16 15:07:16-05'")),
            decimal: "decimal(12, 2)",
        },
        // MySQL's TIME is an interval and its TIMESTAMP has no zone of its own (ADR-006).
        "mysql" | "mariadb" => Types {
            date: "date",
            time: None,
            timestamp: "datetime",
            zoned: None,
            decimal: "decimal(12, 2)",
        },
        "sqlserver" => Types {
            date: "date",
            time: Some("time"),
            timestamp: "datetime2",
            zoned: Some(("datetimeoffset", "'2026-11-16 15:07:16 -05:00'")),
            decimal: "decimal(12, 2)",
        },
        // SQLite keeps what it is given: declared types say what the text is.
        _ => Types {
            date: "date",
            time: None,
            timestamp: "datetime",
            zoned: None,
            decimal: "numeric",
        },
    }
}

/// The table every check reads: two rows, so a filter that matches must match one of them.
async fn table(label: &str, session: &Session, name: &str) -> ResultId {
    let t = types(label);
    run(label, session, &[&format!("DROP TABLE {name}")], true).await;
    let mut columns = vec![
        "id INT PRIMARY KEY".to_string(),
        format!("d {}", t.date),
        format!("ts {}", t.timestamp),
        format!("n {}", t.decimal),
        "big BIGINT".to_string(),
    ];
    let mut first = vec![
        "1",
        "'2026-11-16'",
        "'2026-11-16 20:07:16'",
        "1234567.89",
        "1234567",
    ];
    let mut second = vec!["2", "'2025-01-02'", "'2025-01-02 03:04:05'", "-0.50", "-42"];
    if let Some(time) = t.time {
        columns.push(format!("t {time}"));
        first.push("'20:07:16'");
        second.push("'03:04:05'");
    }
    if let Some((zoned, literal)) = t.zoned {
        columns.push(format!("tz {zoned}"));
        first.push(literal);
        second.push(match label {
            "sqlserver" => "'2025-01-02 03:04:05 +00:00'",
            _ => "'2025-01-02 03:04:05+00'",
        });
    }
    let create = format!("CREATE TABLE {name} ({})", columns.join(", "));
    let insert = format!(
        "INSERT INTO {name} VALUES ({}), ({})",
        first.join(", "),
        second.join(", ")
    );
    run(label, session, &[&create, &insert], false).await;
    run(
        label,
        session,
        &[&format!("SELECT * FROM {name} ORDER BY id")],
        false,
    )
    .await
    .unwrap_or_else(|| panic!("{label}: no result"))
}

fn madrid() -> Display {
    DisplayFormat {
        as_stored: false,
        date: "%d/%m/%Y".into(),
        time: "%H.%M".into(),
        timestamp: "%d/%m/%Y %H:%M".into(),
        grouping: Grouping::Dot,
        decimal: DecimalMark::Comma,
        time_zone: "Europe/Madrid".into(),
        binary: BinaryDisplay::Hex,
    }
    .compile()
    .unwrap()
}

/// A window as the grid receives it: per column, each row's text as the database prints it
/// and as it is shown (`None`: the same), and the display column's kind and zone.
struct Window {
    names: Vec<String>,
    stored: Vec<Vec<Option<String>>>,
    shown: Vec<Vec<Option<String>>>,
    kinds: Vec<String>,
    zones: Vec<Option<String>>,
}

impl Window {
    fn read(bytes: &[u8]) -> Self {
        let reader = StreamReader::try_new(Cursor::new(bytes), None).unwrap();
        let schema = reader.schema();
        let half = schema.fields().len() / 2;
        let mut window = Window {
            names: schema.fields()[..half]
                .iter()
                .map(|f| f.name().clone())
                .collect(),
            stored: vec![Vec::new(); half],
            shown: vec![Vec::new(); half],
            kinds: schema.fields()[half..]
                .iter()
                .map(|f| f.metadata().get(KIND_KEY).cloned().unwrap_or_default())
                .collect(),
            zones: schema.fields()[half..]
                .iter()
                .map(|f| f.metadata().get(ZONE_KEY).cloned())
                .collect(),
        };
        for batch in reader {
            let batch = batch.unwrap();
            for column in 0..half * 2 {
                let text = batch
                    .column(column)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("every cell is text");
                let cells = (0..text.len())
                    .map(|row| (!text.is_null(row)).then(|| text.value(row).to_string()));
                if column < half {
                    window.stored[column].extend(cells);
                } else {
                    window.shown[column - half].extend(cells);
                }
            }
        }
        window
    }

    fn column(&self, name: &str) -> usize {
        self.names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("no column {name} in {:?}", self.names))
    }
}

#[tokio::test]
async fn each_engines_dates_times_and_decimals_are_shown_in_the_display_format() {
    let display = madrid();
    for (label, session, _) in engines(&[]).await {
        let id = table(label, &session, "vd_types").await;
        let stored = session.results().get(id).unwrap();
        let window = {
            let stored = stored.read().unwrap();
            Window::read(&window_ipc(&stored, 0, 10, &display).unwrap())
        };
        let shown = |name: &str, row: usize| -> Option<String> {
            window.shown[window.column(name)][row].clone()
        };
        let kind = |name: &str| window.kinds[window.column(name)].as_str();
        let t = types(label);

        // The database's own text is always there, untouched.
        let as_stored = {
            let stored = stored.read().unwrap();
            Window::read(&window_ipc(&stored, 0, 10, &Display::default()).unwrap())
        };
        assert_eq!(window.stored, as_stored.stored, "{label}");
        assert!(
            as_stored.shown.iter().flatten().all(Option::is_none),
            "{label}: nothing is shown otherwise by default"
        );

        assert_eq!(kind("d"), "date", "{label}");
        assert_eq!(shown("d", 0).as_deref(), Some("16/11/2026"), "{label}");
        assert_eq!(shown("d", 1).as_deref(), Some("02/01/2025"), "{label}");

        assert_eq!(kind("ts"), "timestamp", "{label}");
        assert_eq!(
            shown("ts", 0).as_deref(),
            Some("16/11/2026 20:07"),
            "{label}"
        );
        // A timestamp without a zone is never converted, and has no zone to name.
        assert_eq!(window.zones[window.column("ts")], None, "{label}");

        assert_eq!(kind("n"), "number", "{label}");
        assert_eq!(shown("n", 0).as_deref(), Some("1.234.567,89"), "{label}");
        assert_eq!(kind("big"), "number", "{label}");
        assert_eq!(shown("big", 0).as_deref(), Some("1.234.567"), "{label}");
        // Nothing to group, nothing to show otherwise.
        assert_eq!(shown("big", 1), None, "{label}");

        if t.time.is_some() {
            assert_eq!(kind("t"), "time", "{label}");
            assert_eq!(shown("t", 0).as_deref(), Some("20.07"), "{label}");
        }
        if t.zoned.is_some() {
            assert_eq!(kind("tz"), "timestamptz", "{label}");
            // 20:07 UTC is 21:07 in Madrid in November, and the cell says its offset.
            assert_eq!(
                shown("tz", 0).as_deref(),
                Some("16/11/2026 21:07 +01:00"),
                "{label}"
            );
            assert_eq!(
                shown("tz", 1).as_deref(),
                Some("02/01/2025 04:04 +01:00"),
                "{label}"
            );
            assert_eq!(
                window.zones[window.column("tz")].as_deref(),
                Some("Europe/Madrid"),
                "{label}"
            );
        }

        // "Copy as displayed" is the shown text; the header is the columns' names.
        let copied = {
            let stored = stored.read().unwrap();
            displayed_text(
                &stored,
                &[0],
                &[window.column("d"), window.column("n")],
                true,
                &display,
            )
        };
        let lines: Vec<&str> = copied.lines().collect();
        assert_eq!(lines.len(), 2, "{label}: {copied:?}");
        assert_eq!(lines[1], "16/11/2026\t1.234.567,89", "{label}");
        // Rows that are not next to each other (a selection of single rows): each one, in
        // the order given, and nothing between them.
        let picked = {
            let stored = stored.read().unwrap();
            displayed_text(&stored, &[1, 0, 9], &[window.column("d")], false, &display)
        };
        assert_eq!(picked, "02/01/2025\n16/11/2026\n", "{label}");
        run(label, &session, &["DROP TABLE vd_types"], true).await;
    }
}

/// "Filter by this value" from a cell that is shown formatted: the core writes the condition
/// from the stored value, so it keeps exactly the row the cell is in.
#[tokio::test]
async fn a_filter_written_from_a_formatted_cell_still_matches_its_row() {
    let display = madrid();
    for (label, session, schema) in engines(&[]).await {
        let id = table(label, &session, "vd_filter").await;
        let stored = session.results().get(id).unwrap();
        let dialect = session.dialect();
        let (columns, window) = {
            let stored = stored.read().unwrap();
            (
                stored.schema().columns.clone(),
                Window::read(&window_ipc(&stored, 0, 10, &display).unwrap()),
            )
        };
        let mut formatted = 0;
        for (column, meta) in columns.iter().enumerate() {
            for row in 0..2 {
                let Some(shown) = window.shown[column][row].clone() else {
                    continue;
                };
                formatted += 1;
                let value: Value = stored.read().unwrap().value_at(row, column).unwrap();
                // The shown text is not what the database prints: it would not be found.
                assert_ne!(shown, value.to_string(), "{label} {}", meta.name);
                let condition = value_condition(
                    dialect.as_ref(),
                    &meta.name,
                    meta.logical,
                    FilterOp::Eq,
                    &value,
                )
                .unwrap_or_else(|e| panic!("{label} {}: {e}", meta.name));
                let filter = add_condition(dialect.as_ref(), "", &condition).unwrap();
                let rows = count_table_rows(&session, schema, "vd_filter", &filter)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {filter}\n→ {e}"));
                assert_eq!(rows, 1, "{label}: {filter} (shown as {shown:?})");
            }
        }
        assert!(
            formatted >= 6,
            "{label}: only {formatted} cells were formatted"
        );
        run(label, &session, &["DROP TABLE vd_filter"], true).await;
    }
}

/// A masked column (ADR-051) is the mask, whatever its type and the display format: it is
/// never formatted, in a window or in a copy as displayed.
#[tokio::test]
async fn a_masked_column_is_never_formatted() {
    let display = madrid();
    for (label, session, _) in engines(&["ts", "n"]).await {
        let id = table(label, &session, "vd_masked").await;
        let stored = session.results().get(id).unwrap();
        // What the grid receives and what a copy as displayed gives, read under the lock;
        // the checks follow without it.
        let (window, masked, copied) = {
            let stored = stored.read().unwrap();
            let window = Window::read(&window_ipc(&stored, 0, 10, &display).unwrap());
            let masked: Vec<bool> = (0..window.names.len())
                .map(|column| stored.is_masked(column))
                .collect();
            let all: Vec<usize> = (0..window.names.len()).collect();
            let copied = displayed_text(&stored, &[0, 1], &all, false, &display);
            (window, masked, copied)
        };
        for name in ["ts", "n"] {
            let column = window.column(name);
            assert!(masked[column], "{label}: {name} is masked");
            assert_eq!(
                window.stored[column],
                [Some(MASK.to_string()), Some(MASK.to_string())],
                "{label} {name}"
            );
            assert_eq!(window.shown[column], [None, None], "{label} {name}");
            assert_eq!(window.kinds[column], "other", "{label} {name}");
            assert_eq!(window.zones[column], None, "{label} {name}");
        }
        // The other columns are still formatted.
        assert_eq!(
            window.shown[window.column("d")][0].as_deref(),
            Some("16/11/2026"),
            "{label}"
        );
        for secret in ["20:07", "1.234.567,89", "1234567.89", "03:04"] {
            // The time columns of other, unmasked columns may hold the hour: only what the
            // masked columns alone hold is looked for.
            if secret.contains(':') && types(label).time.is_some() {
                continue;
            }
            if secret.contains(':') && types(label).zoned.is_some() {
                continue;
            }
            assert!(!copied.contains(secret), "{label}: {secret} in {copied:?}");
        }
        assert_eq!(copied.matches(MASK).count(), 4, "{label}: {copied:?}");
        run(label, &session, &["DROP TABLE vd_masked"], true).await;
    }
}
