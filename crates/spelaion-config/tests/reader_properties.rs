//! Property tests of the readers of other clients' files and of pasted connections (SDB-156):
//! whatever the file holds, they never panic. Each case starts from a real file of the client
//! (the fixtures the example tests read) and changes it: bytes replaced, cut, repeated or
//! removed, and hostile pieces put in (huge numbers, deep nesting, broken UTF-8, markup, the
//! pieces of each format). Beekeeper's SQLite database gets any value in any column instead.
//!
//! In CI the cases are bounded and the seed is fixed, so a run is the same every time and a
//! failure prints its smallest input. `SPELAION_FUZZ_CASES=<n>` runs `n` cases per property
//! with a random seed (`PROPTEST_RNG_SEED` fixes it), for long runs on a machine or on a
//! schedule, never in a pull request's job.

mod common;

use std::path::{Path, PathBuf};

use common::{FakeBackend, Fixture};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{Config, RngSeed};
use spelaion_config::{
    WorkbenchPasswords, paste_connection, read_beekeeper, read_datagrip, read_dbeaver,
    read_heidisql, read_pgadmin, read_tableplus, read_workbench,
};
use spelaion_driver_api::Secret;

fn config(cases: u32) -> Config {
    let fuzz = std::env::var("SPELAION_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok());
    match fuzz {
        Some(cases) => Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        },
        None => Config {
            cases,
            rng_seed: RngSeed::Fixed(0x5DB_0156),
            failure_persistence: None,
            ..Config::default()
        },
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A folder of its own for one case, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "spelaion-reader-properties-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

// --------------------------------------------------------------------------------- mutations

/// Pieces that break readers: numbers out of range, nesting, broken or unusual text, and the
/// syntax of JSON, XML, property lists and HeidiSQL's export.
fn nasty() -> BoxedStrategy<Vec<u8>> {
    const PIECES: &[&str] = &[
        "\0",
        "\u{feff}",
        "é",
        "🦀",
        "\u{202e}",
        "-1",
        "0",
        "65536",
        "99999999999999999999",
        "-9223372036854775809",
        "1e999",
        "NaN",
        "\"",
        "'",
        "\\",
        "\\u0000",
        "\\ud800",
        "<",
        ">",
        "&",
        "&#0;",
        "&#xD800;",
        "&unknown;",
        "]]>",
        "<![CDATA[",
        "<!--",
        "-->",
        "<!DOCTYPE x [",
        "{",
        "}",
        "[",
        "]",
        ":",
        ",",
        "null",
        "true",
        "{}",
        "[]",
        "\"\"",
        "<key>",
        "</key>",
        "<string>",
        "<dict>",
        "</dict>",
        "<array/>",
        "<integer>-1</integer>",
        "<data>!!</data>",
        "<|||>",
        "\\n",
        "\n",
        "\r",
        "\t",
        "../../",
        "%00",
        "${env:HOME}",
        "cmd:x",
        "=",
        "[x]",
    ];
    prop_oneof![
        6 => prop::sample::select(PIECES).prop_map(|s| s.as_bytes().to_vec()),
        1 => prop::collection::vec(any::<u8>(), 1..16),
        1 => (1..4usize, prop::sample::select(vec!["[", "{", "<dict>", "<a>", "<array>"]))
            .prop_map(|(k, s)| s.repeat(k * 200).into_bytes()),
        1 => Just("a".repeat(70_000).into_bytes()),
    ]
    .boxed()
}

#[derive(Debug, Clone)]
enum Mutation {
    Replace(Index, u8),
    Insert(Index, Vec<u8>),
    Delete(Index, usize),
    Repeat(Index, usize),
    Cut(Index),
}

fn mutations() -> BoxedStrategy<Vec<Mutation>> {
    let one = prop_oneof![
        3 => (any::<Index>(), any::<u8>()).prop_map(|(i, b)| Mutation::Replace(i, b)),
        4 => (any::<Index>(), nasty()).prop_map(|(i, b)| Mutation::Insert(i, b)),
        2 => (any::<Index>(), 1..64usize).prop_map(|(i, n)| Mutation::Delete(i, n)),
        1 => (any::<Index>(), 1..256usize).prop_map(|(i, n)| Mutation::Repeat(i, n)),
        1 => any::<Index>().prop_map(Mutation::Cut),
    ];
    prop::collection::vec(one, 1..6).boxed()
}

fn mutate(seed: &[u8], mutations: &[Mutation]) -> Vec<u8> {
    let mut bytes = seed.to_vec();
    for mutation in mutations {
        let at = |i: &Index, len: usize| if len == 0 { 0 } else { i.index(len) };
        match mutation {
            Mutation::Replace(i, b) => {
                if !bytes.is_empty() {
                    let p = at(i, bytes.len());
                    bytes[p] = *b;
                }
            }
            Mutation::Insert(i, piece) => {
                let p = at(i, bytes.len() + 1);
                bytes.splice(p..p, piece.iter().copied());
            }
            Mutation::Delete(i, n) => {
                if !bytes.is_empty() {
                    let p = at(i, bytes.len());
                    let end = (p + n).min(bytes.len());
                    bytes.drain(p..end);
                }
            }
            Mutation::Repeat(i, n) => {
                if !bytes.is_empty() {
                    let p = at(i, bytes.len());
                    let end = (p + n).min(bytes.len());
                    let slice = bytes[p..end].to_vec();
                    bytes.splice(end..end, slice);
                }
            }
            Mutation::Cut(i) => {
                let p = at(i, bytes.len() + 1);
                bytes.truncate(p);
            }
        }
    }
    bytes
}

/// A file of the fixture under `dir` replaced by its mutated bytes.
fn mutate_file(dir: &Path, relative: &str, mutations: &[Mutation]) {
    let path = dir.join(relative);
    let seed = std::fs::read(&path).unwrap();
    std::fs::write(&path, mutate(&seed, mutations)).unwrap();
}

// ---------------------------------------------------------------------------------- readers

/// A DBeaver project's `data-sources.json`, like the one DBeaver writes.
const DBEAVER_SOURCES: &str = r#"{
  "folders": {"Prod": {}},
  "connection-types": {
    "prod": {"name": "Production", "color": "247,159,129", "confirm-execute": true,
      "auto-commit": false, "close-connections-period": 900}
  },
  "connections": {
    "postgres-jdbc-1": {
      "provider": "postgresql", "driver": "postgres-jdbc", "name": "Orders PG",
      "description": "Orders", "tags": {"team": "", "region": "eu"},
      "folder": "Prod", "save-password": true,
      "configuration": {
        "host": "db.internal", "port": "5432", "database": "orders", "type": "prod",
        "url": "jdbc:postgresql://db.internal:5432/orders",
        "handlers": {"ssh_tunnel": {"type": "TUNNEL", "enabled": true,
          "properties": {"host": "bastion.example.com", "port": 22, "authType": "PUBLIC_KEY",
            "keyPath": "~/.ssh/id_ed25519"}}},
        "bootstrap": {"initQueries": ["set role reader"], "defaultSchema": "public"},
        "provider-properties": {"@dbeaver-show-non-default-db@": "true", "sslmode": "require"}
      }
    },
    "mysql8-2": {
      "provider": "mysql", "driver": "mysql8", "name": "Shop",
      "configuration": {"host": "127.0.0.1", "port": "13306", "database": "shop",
        "auth-model": "native", "user": "root"}
    },
    "sqlite-3": {
      "provider": "sqlite", "driver": "sqlite_jdbc", "name": "File",
      "configuration": {"database": "C:/data/shop.db", "url": "jdbc:sqlite:C:/data/shop.db"}
    }
  }
}"#;

/// What can be pasted as a connection: a copied connection, connection URLs.
const PASTE_SEEDS: &[&str] = &[
    "schema_version = 1\nname = \"Prod\"\nengine = \"postgres\"\nhost = \"db.example.com\"\n\
     port = 5432\ndatabase = \"d\"\nuser = \"u\"\npassword = { secret = \"prompt\" }\n\
     [tls]\nmode = \"verify-full\"\n[ssh]\ntunnel = \"bastion\"\n[masking]\ncolumns = [\"email\"]\n",
    "postgres://reader:s3cret@db.example.com:5433/orders?sslmode=verify-full",
    "mysql://root@localhost",
    "sqlite:C:/data/shop.v2.sqlite",
    "sqlserver://sa@127.0.0.1:11433;database=master",
];

proptest! {
    #![proptest_config(config(64))]

    #[test]
    fn dbeaver_reads_any_project_without_panicking(
        sources in mutations(),
        credentials in prop::collection::vec(any::<u8>(), 0..96),
    ) {
        let scratch = Scratch::new();
        let dir = scratch.0.join("General").join(".dbeaver");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data-sources.json"), mutate(DBEAVER_SOURCES.as_bytes(), &sources)).unwrap();
        std::fs::write(dir.join("credentials-config.json"), credentials).unwrap();
        let _ = read_dbeaver(&scratch.0);
    }

    #[test]
    fn datagrip_reads_any_project_without_panicking(
        file in prop::sample::select(vec![
            "DataGripProjects/default/.idea/dataSources.xml",
            "DataGripProjects/default/.idea/dataSources.local.xml",
            "JetBrains/DataGrip2026.3/options/recentProjects.xml",
            "JetBrains/DataGrip2026.3/options/sshConfigs.xml",
        ]),
        changes in mutations(),
    ) {
        let scratch = Scratch::new();
        copy_dir(&fixtures().join("datagrip"), &scratch.0);
        mutate_file(&scratch.0, file, &changes);
        let _ = read_datagrip(
            &scratch.0.join("DataGripProjects/default"),
            &[scratch.0.join("JetBrains/DataGrip2026.3")],
        );
    }

    #[test]
    fn tableplus_reads_any_file_without_panicking(
        file in prop::sample::select(vec![
            "com.tinyapp.TablePlus/Data/Connections.plist",
            "com.tinyapp.TablePlus/Data/ConnectionGroups.plist",
            "export.tableplusconnection",
            "export.json",
        ]),
        changes in mutations(),
        password in prop::option::of(prop_oneof![Just("fixture-export-pw".to_string()), any::<String>()]),
    ) {
        let scratch = Scratch::new();
        copy_dir(&fixtures().join("tableplus"), &scratch.0);
        mutate_file(&scratch.0, file, &changes);
        let target = if file.starts_with("com.") {
            scratch.0.join("com.tinyapp.TablePlus")
        } else {
            scratch.0.join(file)
        };
        let _ = read_tableplus(&target, password.map(Secret::new).as_ref());
    }

    #[test]
    fn workbench_reads_any_file_without_panicking(changes in mutations()) {
        let scratch = Scratch::new();
        copy_dir(&fixtures().join("workbench"), &scratch.0);
        mutate_file(&scratch.0, "Workbench/connections.xml", &changes);
        for passwords in [WorkbenchPasswords::Vault, WorkbenchPasswords::SecretService] {
            let _ = read_workbench(&scratch.0.join("Workbench"), passwords);
        }
    }

    #[test]
    fn pgadmin_reads_any_export_without_panicking(changes in mutations()) {
        let scratch = Scratch::new();
        copy_dir(&fixtures().join("pgadmin"), &scratch.0);
        mutate_file(&scratch.0, "pgadmin-9.19-servers.json", &changes);
        let _ = read_pgadmin(&scratch.0.join("pgadmin-9.19-servers.json"));
    }

    #[test]
    fn heidisql_reads_any_export_without_panicking(changes in mutations()) {
        let scratch = Scratch::new();
        copy_dir(&fixtures().join("heidisql"), &scratch.0);
        mutate_file(&scratch.0, "heidisql-12.22.txt", &changes);
        let _ = read_heidisql(&scratch.0.join("heidisql-12.22.txt"));
    }

    /// Beekeeper's database: any value of any type in the columns it reads, and a broken key.
    #[test]
    fn beekeeper_reads_any_database_without_panicking(
        row in 1..9i64,
        values in prop::collection::vec(sqlite_value(), 12),
        key in prop_oneof![Just(None), mutations().prop_map(Some)],
        file_changes in prop::option::of(mutations()),
    ) {
        let scratch = Scratch::new();
        let dir = scratch.0.join("beekeeper-studio");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let db = rusqlite::Connection::open(dir.join("app.db")).unwrap();
            let sql = std::fs::read_to_string(fixtures().join("beekeeper/beekeeper-6.2.2.sql")).unwrap();
            db.execute_batch(&sql).unwrap();
            // A value the schema refuses (NULL where it says NOT NULL) leaves the row as it was.
            let _ = db.execute(
                "update saved_connection set connectionType = ?1, host = ?2, port = ?3, \
                 username = ?4, defaultDatabase = ?5, options = ?6, sshPort = ?7, sshHost = ?8, \
                 labelColor = ?9, connectionFolderId = ?10, password = ?11, sslCaFile = ?12 \
                 where id = ?13",
                rusqlite::params_from_iter(values.iter().cloned().chain([rusqlite::types::Value::Integer(row)])),
            );
        }
        std::fs::copy(fixtures().join("beekeeper/.key"), dir.join(".key")).unwrap();
        if let Some(changes) = key {
            mutate_file(&dir, ".key", &changes);
        }
        if let Some(changes) = file_changes {
            mutate_file(&dir, "app.db", &changes);
        }
        let _ = read_beekeeper(&dir);
    }

    /// Pasted text: a copied connection or a URL, changed, or any text at all.
    #[test]
    fn any_pasted_connection_is_read_without_panicking(
        text in prop_oneof![
            (prop::sample::select(PASTE_SEEDS), mutations())
                .prop_map(|(seed, changes)| String::from_utf8_lossy(&mutate(seed.as_bytes(), &changes)).into_owned()),
            any::<String>(),
        ],
    ) {
        let fx = Fixture::new(&format!("paste-property-{}", Scratch::new().0.file_name().unwrap().to_string_lossy()));
        let backend = FakeBackend::default();
        let _ = paste_connection(&fx.config(), &text, &backend);
        prop_assert!(backend.keychain.lock().unwrap().is_empty(), "a paste wrote to the keychain");
    }
}

/// Any SQLite value: the database is typeless, so a column may hold any of them.
fn sqlite_value() -> BoxedStrategy<rusqlite::types::Value> {
    use rusqlite::types::Value;
    prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Integer),
        any::<f64>().prop_map(Value::Real),
        any::<String>().prop_map(Value::Text),
        prop::sample::select(vec![
            "postgresql",
            "mysql",
            "mariadb",
            "sqlite",
            "sqlserver",
            "redshift",
            "cockroachdb",
            "{}",
            "{\"connectionMethod\":",
            "[1,2",
            "-1",
            "65536",
            "",
        ])
        .prop_map(|s| Value::Text(s.to_string())),
        prop::collection::vec(any::<u8>(), 0..32).prop_map(Value::Blob),
    ]
    .boxed()
}
