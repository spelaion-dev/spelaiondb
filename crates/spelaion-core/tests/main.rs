//! The core's integration tests, linked once (SDB-168): each file of this folder is a module of
//! this one binary instead of a binary of its own, which linked the core, its dependencies and
//! their debug information again for every file. Its files still run one after another, as
//! their binaries did (`serial.rs`): every test starts with
//! `let _turn = crate::serial::file_turn(module_path!());`.
//!
//! `cargo test -p spelaion-core edit_source` runs one file (its tests' names start with it).
//! `hive`, `trino`, `influxdb`, `mysql_family`, `postgres_family` and `search` stay binaries of
//! their own, run apart by name (`--test hive`) in CI's and AGENTS.md's later phases.
//!
//! A new test file is a module here, uses `crate::engines` or `crate::support` instead of
//! declaring them, and takes its turn in each test.

mod engines;
mod serial;
mod support;

mod agents_engines;
mod babelfish;
mod backup;
mod backup_files;
mod bulk_edits;
mod cassandra;
mod cleartext;
mod clickhouse;
mod column_stats;
mod conn_test;
mod copy_engines;
mod copy_from_stdin;
mod copy_same_file;
mod copy_updates;
mod data_search;
mod databend;
mod early_cancel;
mod edit_source;
mod er_diagram;
mod export_formats;
mod export_stream;
mod file_folder;
mod flightsql;
mod generated_sql;
mod geometry;
mod grid_engines;
mod grid_million;
mod health;
mod history;
mod import;
mod import_cancel;
mod imported_setup;
mod kept_results;
mod kube;
mod late_cancel;
mod libsql;
mod masking;
mod mongodb;
mod mysql_dumps;
mod mysql_sql_mode_checks;
mod neo4j;
mod object_actions;
mod object_details;
mod open;
mod oracle;
mod panics;
mod pgbouncer;
mod plans;
mod plans_clickhouse;
mod plans_mysql;
mod plans_sqlite_duckdb;
mod plans_sqlserver;
mod postgres_databases;
mod privileges;
mod properties;
mod query_pages;
mod read_only_connection;
mod readonly;
mod redis;
mod references;
mod result_jumps;
mod run_from_cursor;
mod schema_compare;
mod script_runs;
mod server_dashboard;
mod server_messages;
mod server_observation;
mod session_duckdb;
mod session_mysql;
mod session_postgres;
mod session_read_only;
mod session_setup;
mod session_source;
mod session_sqlite;
mod session_sqlserver;
mod shown_databases;
mod spanner;
mod tdengine;
mod tls_warning;
mod transactions;
mod tsql_batches;
mod users;
mod value_display;
mod value_files;
mod value_pickers;
mod variables;
mod write;
mod xlsx;

/// Every test of this binary takes its file's turn, so no file's tests overlap another's.
#[test]
fn every_test_takes_its_files_turn() {
    let _turn = crate::serial::file_turn(module_path!());
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let turn = "let _turn = crate::serial::file_turn(module_path!());";
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("tests folder") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let own_binary = [
            "hive.rs",
            "trino.rs",
            "influxdb.rs",
            "mysql_family.rs",
            "postgres_family.rs",
            "search.rs",
            "main.rs",
            "serial.rs",
        ];
        if !name.ends_with(".rs") || own_binary.contains(&name.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("test file");
        let module = name.trim_end_matches(".rs");
        assert!(
            include_str!("main.rs").contains(&format!("\nmod {module};\n")),
            "tests/{name} is not a module of tests/main.rs, so it never runs"
        );
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let attribute = line.trim();
            if !(attribute == "#[test]" || attribute.starts_with("#[tokio::test")) {
                continue;
            }
            // The turn is the first statement of the body: the line after the one that opens it.
            let opens = lines[i..]
                .iter()
                .position(|l| l.trim_end().ends_with('{'))
                .map(|at| i + at);
            if opens
                .and_then(|at| lines.get(at + 1))
                .is_none_or(|next| next.trim() != turn)
            {
                missing.push(format!("tests/{name}:{}", i + 1));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these tests do not take their file's turn first:\n{}",
        missing.join("\n")
    );
}
