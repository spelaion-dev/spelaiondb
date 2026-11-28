//! The configuration crate's integration tests, linked once (SDB-168): each file of this folder
//! is a module of this one binary instead of a binary of its own. Its files still run one after
//! another, as their binaries did (`serial.rs`): every test starts with
//! `let _turn = crate::serial::file_turn(module_path!());`.
//!
//! `cargo test -p spelaion-config masking` runs one file (its tests' names start with it). A new
//! test file is a module here, uses `crate::common` instead of declaring it, and takes its turn
//! in each test.

mod common;
mod serial;

mod beekeeper;
mod cassandra;
mod config;
mod connection_transfer;
mod connection_types;
mod copies;
mod datagrip;
mod descriptive_keys;
mod engine_change;
mod engine_names;
mod external_engines;
mod file_writes;
mod flightsql;
mod hive;
mod import_sources;
mod imports;
mod key_passphrases;
mod localdb;
mod masking;
mod mcp_clients;
mod mongodb;
mod moves;
mod network_profiles;
mod pgadmin;
mod poolers;
mod postgres_databases;
mod reader_properties;
mod redis;
mod search;
mod session_setup;
mod several_folders;
mod tableplus;
mod tdengine;
mod trust_updates;
mod workbench;

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
        if !name.ends_with(".rs") || ["main.rs", "serial.rs"].contains(&name.as_str()) {
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
