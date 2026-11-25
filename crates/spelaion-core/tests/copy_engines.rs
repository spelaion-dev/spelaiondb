//! Copying tables between connections (SDB-35, ADR-069), between every pair of the engines a
//! copy can write to: PostgreSQL (`SPELAION_TEST_PG`), MySQL and MariaDB (`SPELAION_TEST_MYSQL`,
//! `SPELAION_TEST_MARIADB`), SQL Server (`SPELAION_TEST_MSSQL`), ClickHouse
//! (`SPELAION_TEST_CLICKHOUSE`), and SQLite and DuckDB (always). The same table, with every
//! common type, is created on each one in its own DDL and copied to each one; what is read back
//! is compared with what was written, value by value, each type compared as itself (a boolean's
//! `1` is `true`, a timestamp with zone is compared at its UTC time, JSON as parsed). A binary
//! column holding every byte goes too, compared as its hex (SDB-464); ClickHouse has no binary
//! type (its `String` is read as text), so its table has none, though it takes one as a target.
//! An engine whose variable is not set is left out of the pairs, which does not count as tested.

use crate::engines;

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::table_copy::{
    Blocked, CopyError, CopyHow, CopyRequest, Loss, plan_copy, run_copy,
};
use spelaion_core::{CoreEvent, IfExists, JobCancel, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

const NONE: Safeguards = Safeguards {
    confirm_execution: false,
    confirm_data_changes: false,
    close_idle_after: None,
    close_idle_transaction_after: None,
    manual_commit: false,
    smart_commit: false,
};

/// The copied table's columns, in order, but its binary one.
const COLUMNS: [&str; 13] = [
    "id", "b", "i2", "i8", "d", "f", "t", "tx", "dt", "ts", "tz", "u", "j",
];

/// The binary column, last, on every engine but ClickHouse (SDB-464).
const BYTES: &str = "bn";

/// Every byte, in order, as hex: the binary column's first value.
fn every_byte() -> String {
    (0..=255u8).map(|b| format!("{b:02x}")).collect()
}

/// The columns of a table, with the binary one or without.
fn columns(with_bytes: bool) -> Vec<&'static str> {
    let mut columns = COLUMNS.to_vec();
    if with_bytes {
        columns.push(BYTES);
    }
    columns
}

/// Whether `side`'s table has the binary column: every engine's but ClickHouse's.
fn has_bytes(side: &Side) -> bool {
    side.session.dialect().name() != "clickhouse"
}

struct Side {
    label: &'static str,
    session: Session,
    schema: &'static str,
}

async fn run(session: &Session, script: &str) {
    for event in session.run_script(script).collect::<Vec<_>>().await {
        if let CoreEvent::StatementFailed { error, .. } = event {
            panic!("{script}: {error:?}");
        }
    }
}

/// Every row of `sql`, the first `width` values of each as text (NULL as `NULL`).
async fn rows(session: &Session, sql: &str, width: usize) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for event in session.run_script(sql).collect::<Vec<_>>().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {error:?}"),
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(result).unwrap();
                let stored = stored.read().unwrap();
                for row in 0..stored.rows() {
                    out.push(
                        (0..width)
                            .map(|c| stored.value_at(row, c).unwrap().to_string())
                            .collect(),
                    );
                }
                drop(stored);
                session.results().remove(result);
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::ResultEnded { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    out
}

fn server(var: &str) -> Option<(String, u16)> {
    let address = std::env::var(var).ok()?;
    let (host, port) = address.rsplit_once(':')?;
    Some((host.into(), port.parse().ok()?))
}

fn session(
    driver: &Arc<dyn Driver>,
    connection: Box<dyn spelaion_driver_api::Connection>,
) -> Session {
    Session::new(
        connection,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

fn folder(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spelaion-copy-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every engine whose server is up, plus SQLite and DuckDB.
async fn sides(label: &str) -> Vec<Side> {
    let mut sides = Vec::new();
    let network =
        |user: &str, password: &str, database: &str, host: String, port: u16| ConnectParams {
            host,
            port,
            user: user.into(),
            password: Some(Secret::new(password)),
            database: database.into(),
            ..Default::default()
        };
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        let driver = engines::postgres();
        let c = driver
            .connect(&network("spelaion", "spelaion", "spelaion", host, port))
            .await
            .unwrap();
        sides.push(Side {
            label: "PostgreSQL",
            session: session(&driver, c),
            schema: "public",
        });
    }
    for (var, driver, label) in [
        ("SPELAION_TEST_MYSQL", engines::mysql(), "MySQL"),
        ("SPELAION_TEST_MARIADB", engines::mariadb(), "MariaDB"),
    ] {
        if let Some((host, port)) = server(var) {
            let c = driver
                .connect(&network("spelaion", "spelaion", "spelaion", host, port))
                .await
                .unwrap();
            sides.push(Side {
                label,
                session: session(&driver, c),
                schema: "spelaion",
            });
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let driver = engines::sqlserver();
        let connect = |database: &str| ConnectParams {
            tls: TlsMode::Require,
            ..network("sa", "Spelaion-test-1", database, host.clone(), port)
        };
        let master = session(&driver, driver.connect(&connect("master")).await.unwrap());
        run(
            &master,
            "if db_id('spelaion') is null create database spelaion",
        )
        .await;
        let c = driver.connect(&connect("spelaion")).await.unwrap();
        sides.push(Side {
            label: "SQL Server",
            session: session(&driver, c),
            schema: "dbo",
        });
    }
    if let Some((host, port)) = server("SPELAION_TEST_CLICKHOUSE") {
        let driver = engines::clickhouse();
        let c = driver
            .connect(&ConnectParams {
                tls: TlsMode::Disable,
                ..network("spelaion", "spelaion", "spelaion", host, port)
            })
            .await
            .unwrap();
        sides.push(Side {
            label: "ClickHouse",
            session: session(&driver, c),
            schema: "spelaion",
        });
    }
    let dir = folder(label);
    let sqlite = dir.join("copy.db");
    std::fs::File::create(&sqlite).unwrap();
    let driver = engines::sqlite();
    let c = driver
        .connect(&ConnectParams {
            database: sqlite.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    sides.push(Side {
        label: "SQLite",
        session: session(&driver, c),
        schema: "main",
    });
    let duckdb = dir.join("copy.duckdb");
    engines::create_database(spelaion_config::Engine::Duckdb, &duckdb).await;
    let driver = engines::duckdb();
    let c = driver
        .connect(&ConnectParams {
            database: duckdb.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    sides.push(Side {
        label: "DuckDB",
        session: session(&driver, c),
        schema: "main",
    });
    sides
}

/// Another set of sessions to the same servers, and to other SQLite and DuckDB files.
async fn sides_again(label: &str) -> Vec<Side> {
    sides(label).await
}

/// The source table in `side`'s own DDL, with its three rows.
fn source_script(side: &Side, table: &str) -> Vec<String> {
    let engine = side.session.dialect().name();
    let create = match (engine, side.label) {
        ("postgres", _) => format!(
            "create table {table} (id integer primary key, b boolean, i2 smallint, i8 bigint, \
             d numeric(18,4), f double precision, t varchar(40), tx text, dt date, ts timestamp, \
             tz timestamptz, u uuid, j json, bn bytea)"
        ),
        ("mysql", _) => format!(
            "create table {table} (id int primary key, b tinyint(1), i2 smallint, i8 bigint, \
             d decimal(18,4), f double, t varchar(40), tx longtext, dt date, ts datetime(6), \
             tz datetime(6), u char(36), j json, bn longblob)"
        ),
        ("sqlserver", _) => format!(
            "create table {table} (id int primary key, b bit, i2 smallint, i8 bigint, \
             d decimal(18,4), f float, t nvarchar(40), tx nvarchar(max), dt date, \
             ts datetime2(6), tz datetimeoffset(6), u uniqueidentifier, j nvarchar(max), \
             bn varbinary(max))"
        ),
        ("clickhouse", _) => format!(
            "create table {table} (id Int32, b Nullable(Bool), i2 Nullable(Int16), \
             i8 Nullable(Int64), d Nullable(Decimal(18, 4)), f Nullable(Float64), \
             t Nullable(String), tx Nullable(String), dt Nullable(Date32), \
             ts Nullable(DateTime64(6)), tz Nullable(DateTime64(6, 'UTC')), u Nullable(UUID), \
             j Nullable(String)) engine = MergeTree order by id"
        ),
        ("sqlite", _) => format!(
            "create table {table} (id INTEGER PRIMARY KEY, b BOOLEAN, i2 SMALLINT, i8 BIGINT, \
             d DECIMAL(18,4), f REAL, t VARCHAR(40), tx TEXT, dt DATE, ts TIMESTAMP, \
             tz TIMESTAMPTZ, u TEXT, j TEXT, bn BLOB)"
        ),
        _ => format!(
            "create table {table} (id INTEGER PRIMARY KEY, b BOOLEAN, i2 SMALLINT, i8 BIGINT, \
             d DECIMAL(18,4), f DOUBLE, t VARCHAR(40), tx VARCHAR, dt DATE, ts TIMESTAMP, \
             tz TIMESTAMPTZ, u UUID, j JSON, bn BLOB)"
        ),
    };
    let (yes, no) = match engine {
        "mysql" | "sqlserver" | "sqlite" => ("1", "0"),
        _ => ("true", "false"),
    };
    let unicode = if engine == "sqlserver" { "N" } else { "" };
    let zone = |stamp: &str| match engine {
        "sqlserver" => format!("'{stamp} +00:00'"),
        "postgres" | "duckdb" => format!("'{stamp}+00'"),
        "sqlite" => format!("'{stamp}+00:00'"),
        // A zone they cannot keep: the UTC time.
        _ => format!("'{stamp}'"),
    };
    // The binary column's value in each engine's SQL, from hex (SDB-464).
    let bytes = |hex: &str| match engine {
        "postgres" => format!(", decode('{hex}', 'hex')"),
        "mysql" | "sqlite" => format!(", X'{hex}'"),
        "sqlserver" => format!(", 0x{hex}"),
        "duckdb" => format!(", from_hex('{hex}')"),
        _ => String::new(),
    };
    let null_bytes = if has_bytes(side) { ", null" } else { "" };
    let rows = [
        format!(
            "(1, {yes}, -32768, 9223372036854775807, -12345678901234.5678, 0.1, \
             {unicode}'ñandú ''q'' \"x\"', {unicode}'line1\nline2\ttab', '2024-02-29', \
             '2024-02-29 13:45:30.123456', {}, '550e8400-e29b-41d4-a716-446655440000', \
             '{{\"a\": [1, 2], \"b\": \"x\"}}'{})",
            zone("2024-02-29 13:45:30.123456"),
            bytes(&every_byte())
        ),
        format!(
            "(2, null, null, null, null, null, null, null, null, null, null, null, null\
             {null_bytes})"
        ),
        format!(
            "(3, {no}, 0, -1, 0, -2.5e-10, '', 'x', '1970-01-01', '1970-01-01 00:00:00', {}, \
             '6ba7b810-9dad-11d1-80b4-00c04fd430c8', '[]'{})",
            zone("1999-12-31 23:59:59.5"),
            bytes("")
        ),
    ];
    let mut script = vec![create];
    for row in rows {
        script.push(format!("insert into {table} values {row}"));
    }
    script
}

/// A value read back, as what it holds: `column` says how to compare it.
fn normalized(column: &str, value: &str) -> String {
    if value == "NULL" {
        return value.into();
    }
    match column {
        "b" => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "t" => "true".into(),
            "0" | "false" | "f" => "false".into(),
            other => format!("bool? {other}"),
        },
        "d" => {
            let trimmed = if value.contains('.') {
                value.trim_end_matches('0').trim_end_matches('.')
            } else {
                value
            };
            if trimmed == "-0" {
                "0".into()
            } else {
                trimmed.into()
            }
        }
        "f" => {
            let x: f64 = value.parse().unwrap_or(f64::NAN);
            format!("{:.12e}", x)
        }
        "dt" => value.get(..10).unwrap_or(value).into(),
        "ts" | "tz" => utc(value),
        "u" | "bn" => value.to_ascii_lowercase(),
        "j" => serde_json::from_str::<serde_json::Value>(value)
            .map(|v| v.to_string())
            .unwrap_or_else(|_| format!("json? {value}")),
        _ => value.into(),
    }
}

/// A timestamp, with or without a zone, as its UTC time `YYYY-MM-DD HH:MM:SS.ffffff`.
fn utc(value: &str) -> String {
    let value = value.replace('T', " ");
    let value = value.trim();
    // The zone: `+00`, `+00:00`, ` +00:00`, `Z`.
    let (local, offset_minutes) = match value.rfind(['+', '-']).filter(|&i| i > 10) {
        Some(i) => {
            let zone = value[i..].trim();
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            let hours: i64 = digits.get(..2).and_then(|h| h.parse().ok()).unwrap_or(0);
            let minutes: i64 = digits.get(2..4).and_then(|m| m.parse().ok()).unwrap_or(0);
            (value[..i].trim(), sign * (hours * 60 + minutes))
        }
        None => (value.trim_end_matches('Z').trim(), 0),
    };
    let (date, time) = local.split_once(' ').unwrap_or((local, "00:00:00"));
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let fraction = format!("{fraction:0<6}");
    let parsed = chrono::NaiveDateTime::parse_from_str(
        &format!("{date} {clock}.{}", &fraction[..6]),
        "%Y-%m-%d %H:%M:%S%.6f",
    );
    match parsed {
        Ok(stamp) => (stamp - chrono::Duration::minutes(offset_minutes))
            .format("%Y-%m-%d %H:%M:%S%.6f")
            .to_string(),
        Err(_) => format!("timestamp? {value}"),
    }
}

/// A row read back, each value as [`normalized`] writes it. Where SQLite holds the table
/// (`approximate`), its decimals are its own numbers (ADR-069's `DecimalApproximate`): they are
/// compared at the 15 significant digits a double keeps.
fn normalized_row(row: &[String], approximate: bool) -> Vec<String> {
    row.iter()
        .enumerate()
        .map(|(c, v)| {
            let column = COLUMNS.get(c).copied().unwrap_or(BYTES);
            let value = normalized(column, v);
            if approximate && column == "d" && value != "NULL" {
                format!("{:.14e}", value.parse::<f64>().unwrap_or(f64::NAN))
            } else {
                value
            }
        })
        .collect()
}

/// What every engine's table holds, as [`normalized_row`] writes it, with the binary column or
/// without.
fn expected(approximate: bool, with_bytes: bool) -> Vec<Vec<String>> {
    let raw = [
        [
            "1",
            "true",
            "-32768",
            "9223372036854775807",
            "-12345678901234.5678",
            "0.1",
            "ñandú 'q' \"x\"",
            "line1\nline2\ttab",
            "2024-02-29",
            "2024-02-29 13:45:30.123456",
            "2024-02-29 13:45:30.123456+00",
            "550e8400-e29b-41d4-a716-446655440000",
            "{\"a\": [1, 2], \"b\": \"x\"}",
        ],
        [
            "2", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL", "NULL",
            "NULL", "NULL",
        ],
        [
            "3",
            "false",
            "0",
            "-1",
            "0",
            "-2.5e-10",
            "",
            "x",
            "1970-01-01",
            "1970-01-01 00:00:00",
            "1999-12-31 23:59:59.5+00",
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "[]",
        ],
    ];
    let bytes = [every_byte(), "NULL".to_string(), String::new()];
    raw.iter()
        .zip(bytes)
        .map(|(row, bytes)| {
            let mut row: Vec<String> = row.iter().map(|v| v.to_string()).collect();
            if with_bytes {
                row.push(bytes);
            }
            normalized_row(&row, approximate)
        })
        .collect()
}

/// The table's rows on `side`, the binary column as its hex in that engine's SQL.
fn select(side: &Side, table: &str, with_bytes: bool) -> String {
    let mut list = COLUMNS.join(", ");
    if with_bytes {
        let hex = match side.session.dialect().name() {
            "postgres" => "encode(bn, 'hex')",
            "sqlserver" => "lower(convert(varchar(max), bn, 2))",
            _ => "lower(hex(bn))",
        };
        // SQLite's `hex(NULL)` is the empty text.
        list.push_str(&format!(
            ", case when bn is null then null else {hex} end as bn"
        ));
    }
    format!("select {list} from {table} order by id")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_table_of_every_common_type_copies_between_every_pair_of_engines() {
    let _turn = crate::serial::file_turn(module_path!());
    // Each side on a session of its own, as a copy always is: the targets on other files.
    let sides = sides("pairs-source").await;
    let targets = sides_again("pairs-target").await;
    let labels: Vec<&str> = sides.iter().map(|s| s.label).collect();
    eprintln!("copying between: {labels:?}");
    let pid = std::process::id();
    let source_table = format!("copy_src_{pid}");
    for side in &sides {
        run(
            &side.session,
            &format!("drop table if exists {source_table}"),
        )
        .await;
        for statement in source_script(side, &source_table) {
            run(&side.session, &statement).await;
        }
        // The table as each engine reads it is the same before anything is copied.
        let approximate = side.label == "SQLite";
        let with_bytes = has_bytes(side);
        let written: Vec<Vec<String>> = rows(
            &side.session,
            &select(side, &source_table, with_bytes),
            columns(with_bytes).len(),
        )
        .await
        .iter()
        .map(|row| normalized_row(row, approximate))
        .collect();
        assert_eq!(
            written,
            expected(approximate, with_bytes),
            "{} as written",
            side.label
        );
    }
    let mut copied = 0;
    for (s, source) in sides.iter().enumerate() {
        for (t, target) in targets.iter().enumerate() {
            let target_table = format!("copy_{s}_{t}_{pid}");
            run(
                &target.session,
                &format!("drop table if exists {target_table}"),
            )
            .await;
            let mut request = CopyRequest {
                source_schema: source.schema.into(),
                source_table: source_table.clone(),
                target_schema: target.schema.into(),
                target_table: target_table.clone(),
                how: CopyHow::NewTable {
                    if_exists: IfExists::Fail,
                    rows: true,
                },
                skip: Vec::new(),
                types: Vec::new(),
                commit_every: 2,
            };
            let pair = format!("{} → {}", source.label, target.label);
            let mut plan = plan_copy(&source.session, &target.session, &request, &NONE, false)
                .await
                .unwrap_or_else(|e| panic!("{pair}: {e}"));
            // The binary column goes as bytes to a connection that loads them, and as hex text
            // to PostgreSQL and DuckDB, which read it as bytes; elsewhere it is refused until it
            // is left out (ADR-069's point 8). Until every plugin pinned loads bytes (SDB-464),
            // such a pair goes without it.
            let loads_bytes = target.session.connection().capabilities().insert_bytes
                || matches!(target.session.dialect().name(), "postgres" | "duckdb");
            let mut with_bytes = has_bytes(source);
            if with_bytes && !loads_bytes {
                assert_eq!(
                    plan.blocked,
                    [(BYTES.to_string(), Blocked::Binary)],
                    "{pair}"
                );
                eprintln!("{pair}: without the binary column (the target loads no bytes)");
                request.skip = vec![BYTES.into()];
                with_bytes = false;
                plan = plan_copy(&source.session, &target.session, &request, &NONE, false)
                    .await
                    .unwrap_or_else(|e| panic!("{pair}: {e}"));
            }
            assert!(plan.blocked.is_empty(), "{pair}: {:?}", plan.blocked);
            // A zone the target cannot keep is said.
            let tz = plan.columns.iter().find(|c| c.name == "tz").unwrap();
            let zoned_source = matches!(tz.kind, spelaion_core::table_copy::Portable::TimestampTz);
            let drops_zone = matches!(target.session.dialect().name(), "mysql" | "clickhouse");
            assert_eq!(
                tz.target.losses.contains(&Loss::ZoneToUtc),
                zoned_source && drops_zone,
                "{pair}: {:?}",
                tz
            );
            let summary = run_copy(
                &source.session,
                &target.session,
                &request,
                &NONE,
                false,
                false,
                &JobCancel::new(),
                &|_| {},
            )
            .await
            .unwrap_or_else(|e| panic!("{pair}: {e}"));
            assert_eq!(summary.rows, 3, "{pair}");
            let approximate = source.label == "SQLite" || target.label == "SQLite";
            let read: Vec<Vec<String>> = rows(
                &target.session,
                &select(target, &target_table, with_bytes),
                columns(with_bytes).len(),
            )
            .await
            .iter()
            .map(|row| normalized_row(row, approximate))
            .collect();
            assert_eq!(read, expected(approximate, with_bytes), "{pair}");
            run(&target.session, &format!("drop table {target_table}")).await;
            copied += 1;
        }
    }
    eprintln!("{copied} pairs copied");
    assert_eq!(copied, sides.len() * targets.len());
    // Through the other sessions: a copy's source session stays read-only where the engine's
    // barrier is a session setting (ClickHouse's `readonly`). The files are temporary.
    for side in &targets {
        run(
            &side.session,
            &format!("drop table if exists {source_table}"),
        )
        .await;
    }
}

/// Binary goes where the target's connection loads bytes (SDB-464) or reads hex text as bytes,
/// and is refused, saying so, elsewhere (ADR-069's point 8); a left-out column lets the rest go.
#[tokio::test(flavor = "multi_thread")]
async fn binary_copies_where_the_target_reads_it_and_is_refused_elsewhere() {
    let _turn = crate::serial::file_turn(module_path!());
    let sources = sides("binary-source").await;
    let sides = sides_again("binary-target").await;
    let pid = std::process::id();
    let source_table = format!("copy_bin_{pid}");
    let Some(duck) = sources.iter().find(|s| s.label == "DuckDB") else {
        return;
    };
    run(
        &duck.session,
        &format!("drop table if exists {source_table}"),
    )
    .await;
    run(
        &duck.session,
        &format!("create table {source_table} (id INTEGER PRIMARY KEY, payload BLOB)"),
    )
    .await;
    run(
        &duck.session,
        &format!("insert into {source_table} values (1, '\\x00\\x7F\\xFF'::BLOB), (2, NULL)"),
    )
    .await;
    for target in &sides {
        let target_table = format!("copy_bin_to_{}_{pid}", target.session.dialect().name());
        run(
            &target.session,
            &format!("drop table if exists {target_table}"),
        )
        .await;
        let mut request = CopyRequest {
            source_schema: duck.schema.into(),
            source_table: source_table.clone(),
            target_schema: target.schema.into(),
            target_table: target_table.clone(),
            how: CopyHow::NewTable {
                if_exists: IfExists::Fail,
                rows: true,
            },
            skip: Vec::new(),
            types: Vec::new(),
            commit_every: 0,
        };
        // As bytes where the connection loads them (SDB-464), as hex text where the engine reads
        // it as bytes.
        let reads_bytes = target.session.connection().capabilities().insert_bytes
            || matches!(target.session.dialect().name(), "postgres" | "duckdb");
        let outcome = run_copy(
            &duck.session,
            &target.session,
            &request,
            &NONE,
            false,
            false,
            &JobCancel::new(),
            &|_| {},
        )
        .await;
        if reads_bytes {
            outcome.unwrap_or_else(|e| panic!("{}: {e}", target.label));
            let hex = match target.session.dialect().name() {
                "postgres" => {
                    format!("select encode(payload, 'hex') from {target_table} where id = 1")
                }
                "sqlserver" => format!(
                    "select convert(varchar(max), payload, 2) from {target_table} where id = 1"
                ),
                _ => format!("select hex(payload) from {target_table} where id = 1"),
            };
            let read = rows_one(&target.session, &hex).await;
            assert_eq!(read.to_ascii_lowercase(), "007fff", "{}", target.label);
        } else {
            assert!(
                matches!(&outcome, Err(CopyError::Blocked(columns)) if columns == "payload"),
                "{}: {outcome:?}",
                target.label
            );
            // Nothing was created.
            request.skip = vec!["payload".into()];
            run_copy(
                &duck.session,
                &target.session,
                &request,
                &NONE,
                false,
                false,
                &JobCancel::new(),
                &|_| {},
            )
            .await
            .unwrap_or_else(|e| panic!("{} without the column: {e}", target.label));
        }
        run(
            &target.session,
            &format!("drop table if exists {target_table}"),
        )
        .await;
    }
    run(
        &duck.session,
        &format!("drop table if exists {source_table}"),
    )
    .await;
}

/// The first value of `sql`.
async fn rows_one(session: &Session, sql: &str) -> String {
    let mut out = String::new();
    for event in session.run_script(sql).collect::<Vec<_>>().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {error:?}"),
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(result).unwrap();
                let stored = stored.read().unwrap();
                out = stored.value_at(0, 0).unwrap().to_string();
                drop(stored);
                session.results().remove(result);
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::ResultEnded { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    out
}
