//! A folder of CSV, TSV and Parquet files as a DuckDB connection (SDB-316), through DuckDB's
//! plugin: each file is a view, the types are DuckDB's own, and nothing outside the folder is
//! read nor anything written to it, whatever the user runs. Needs nothing external besides the
//! DuckDB plugin (`SPELAION_DEV_PLUGIN_DUCKDB` or the pinned build).

use crate::engines;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Engine, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenError, OpenOptions, ResultStore, TunnelPool,
    open_connection,
};
use spelaion_driver_api::{
    ConnectParams, Connection, ExecEvent, ExecOptions, ObjectKind, ResultSchema, Value,
};
use spelaion_ssh::{KnownHosts, NonInteractive};

struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "spelaion-file-folder-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("repo/data/sales")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.base.join("repo").join(relative)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn connection(&self, id: &str, toml: &str) {
        self.write(
            &format!(".spelaion/connections/{id}.toml"),
            &format!("schema_version = 1\n{toml}"),
        );
    }

    fn problems(&self) -> Vec<String> {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        config.problems.iter().map(|p| p.to_string()).collect()
    }

    async fn open(&self, id: &str) -> Result<OpenConnection, OpenError> {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            })
            .expect("resolving");
        open_connection(
            resolved,
            &Engines::builtin(),
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: self.base.join("user/known_hosts"),
                },
                interaction: Arc::new(NonInteractive),
                password: None,
                read_only_session: false,
            },
            Arc::new(ResultStore::new()),
        )
        .await
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Every file under `dir`, by path, with its length.
fn listing(dir: &Path) -> BTreeSet<(PathBuf, u64)> {
    let mut out = BTreeSet::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                out.insert((entry.path(), meta.len()));
            }
        }
    }
    out
}

/// A path as DuckDB is given it in a test's statements.
fn sql_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "/")
        .replace('\'', "''")
}

/// Runs `sql` in the folder's session: its rows as text, or the error it failed with.
async fn query(
    conn: &OpenConnection,
    sql: &str,
) -> Result<(Arc<ResultSchema>, Vec<Vec<String>>), String> {
    let events: Vec<CoreEvent> = conn.session.run_script(sql).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        return Err(error.message.clone());
    }
    let result = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .ok_or_else(|| format!("{sql}: no result"))?;
    let stored = conn.session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let schema = stored.schema().clone();
    let rows = (0..stored.rows())
        .map(|row| {
            (0..schema.columns.len())
                .map(|column| {
                    stored
                        .value_at(row, column)
                        .map_or_else(String::new, |v| v.to_string())
                })
                .collect()
        })
        .collect();
    Ok((schema, rows))
}

/// The same read on a plain DuckDB session of the plugin: what DuckDB itself makes of a file.
async fn direct(conn: &dyn Connection, sql: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut events = conn.execute(sql, &ExecOptions::default()).await.unwrap();
    let mut natives = Vec::new();
    let mut logicals = Vec::new();
    let mut rows = Vec::new();
    while let Some(event) = events.next().await {
        match event.unwrap() {
            ExecEvent::Columns(schema) => {
                natives = schema
                    .columns
                    .iter()
                    .map(|c| c.native_type.clone())
                    .collect();
                logicals = schema.columns.iter().map(|c| c.logical).collect();
            }
            ExecEvent::Batch(batch) => {
                for row in 0..batch.num_rows() {
                    rows.push(
                        logicals
                            .iter()
                            .enumerate()
                            .map(|(i, logical)| {
                                Value::from_array(batch.column(i).as_ref(), row, *logical)
                                    .map_or_else(String::new, |v| v.to_string())
                            })
                            .collect(),
                    );
                }
            }
            ExecEvent::Notice(_) | ExecEvent::Done { .. } => {}
        }
    }
    (natives, rows)
}

#[tokio::test]
async fn a_folder_opens_as_a_view_of_each_file_with_duckdbs_types() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("views");
    ws.write(
        "data/sales/orders.csv",
        "id,customer,total\n1,Ana,9.50\n2,Luis,3.25\n",
    );
    ws.write("data/sales/lines.tsv", "order_id\tsku\n1\tA-1\n2\tB-2\n");
    ws.write("data/sales/it's here.csv", "note\nquoted\n");
    ws.write("data/sales/notes.txt", "not a table\n");
    ws.write("data/sales/broken.parquet", "this is not parquet");
    // A Parquet file with the types DuckDB's type zoo checks, written by DuckDB itself.
    let parquet = ws.path("data/sales/events.parquet");
    let plain = engines::duckdb()
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let zoo = "select 1::tinyint as t, 2::smallint as s, 3::integer as i, 4::bigint as b, \
               12.34::decimal(10,2) as d, 1.5::double as f, true as yes, \
               date '2026-11-15' as day, timestamp '2026-11-15 10:20:30.123456' as ts, \
               timestamptz '2026-11-15 10:20:30+00' as tz, time '10:20:30' as tm, \
               interval 3 day as span, 'texto ñ' as txt, '\\xAA\\xBB'::blob as bytes, \
               uuid '7b9b0c6e-2f0a-4c4e-9a59-0a4b1d2b6c11' as id, [1, 2] as list, \
               {'k': 1} as st, null::integer as nothing";
    let mut copy = plain
        .execute(
            &format!("copy ({zoo}) to '{}' (format parquet)", sql_path(&parquet)),
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    while let Some(event) = copy.next().await {
        event.unwrap();
    }
    ws.connection(
        "sales",
        "name = \"Sales\"\nengine = \"duckdb\"\ndatabase = \"data/sales\"\nfile_folder = true\n",
    );
    let before = listing(&ws.path("data"));

    let conn = ws.open("sales").await.expect("opening the folder");
    let report = conn.folder.clone().expect("a folder report");
    let views: Vec<_> = report.views.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(
        views,
        ["events", "it's here", "lines", "orders"],
        "{:?}",
        report.skipped
    );
    assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    assert_eq!(report.skipped[0].file, "broken.parquet");

    // The explorer lists them as the folder's relations.
    let objects = conn
        .session
        .connection()
        .list_objects("main")
        .await
        .unwrap();
    let names: BTreeSet<_> = objects.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from(["events", "it's here", "lines", "orders"])
    );
    assert!(objects.iter().all(|o| o.kind == ObjectKind::View));

    let (_, rows) = query(&conn, "select customer, total from orders order by id")
        .await
        .unwrap();
    assert_eq!(rows, [["Ana", "9.5"], ["Luis", "3.25"]]);
    let (_, rows) = query(&conn, "select sku from lines order by order_id")
        .await
        .unwrap();
    assert_eq!(rows, [["A-1"], ["B-2"]]);
    let (_, rows) = query(&conn, "select note from \"it's here\"")
        .await
        .unwrap();
    assert_eq!(rows, [["quoted"]]);

    // The Parquet view gives what DuckDB gives for the file itself: types and every cell.
    let (schema, rows) = query(&conn, "select * from events").await.unwrap();
    let (natives, expected) = direct(
        plain.as_ref(),
        &format!("select * from read_parquet('{}')", sql_path(&parquet)),
    )
    .await;
    let got: Vec<_> = schema
        .columns
        .iter()
        .map(|c| c.native_type.clone())
        .collect();
    assert_eq!(got, natives);
    assert_eq!(rows, expected);
    assert!(
        natives.iter().any(|n| n.starts_with("DECIMAL")),
        "{natives:?}"
    );

    // A file read directly, inside the folder, works too.
    let inside = sql_path(&ws.path("data/sales/orders.csv"));
    let (_, rows) = query(&conn, &format!("select count(*) from '{inside}'"))
        .await
        .unwrap();
    assert_eq!(rows, [["2"]]);

    assert_eq!(listing(&ws.path("data")), before, "nothing is written");
}

#[tokio::test]
async fn nothing_outside_the_folder_is_read_and_nothing_is_written_to_it() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("escape");
    ws.write("data/sales/orders.csv", "id\n1\n");
    // A sibling whose name starts with the folder's, and a file next to the folder.
    ws.write("data/sales2/secret.csv", "secret\nleaked\n");
    ws.write("data/outside.csv", "secret\nleaked\n");
    plain_database(&ws.path("data/other.duckdb")).await;
    // `read_only = false` in the file changes nothing: a folder is always read-only.
    ws.connection(
        "sales",
        "name = \"Sales\"\nengine = \"duckdb\"\ndatabase = \"data/sales\"\nfile_folder = true\nread_only = false\n",
    );
    let before = listing(&ws.path("data"));
    let conn = ws.open("sales").await.expect("opening the folder");
    assert!(conn.resolved.params.read_only);

    let folder = sql_path(&ws.path("data/sales"));
    let sibling = sql_path(&ws.path("data/sales2/secret.csv"));
    let outside = sql_path(&ws.path("data/outside.csv"));
    let other = sql_path(&ws.path("data/other.duckdb"));
    let windows_style = ws
        .path("data/outside.csv")
        .display()
        .to_string()
        .replace('/', "\\");
    for sql in [
        format!("select * from read_csv('{sibling}')"),
        format!("select * from '{sibling}'"),
        format!("select * from read_csv('{outside}')"),
        format!("select * from read_csv('{folder}/../outside.csv')"),
        format!("select * from read_csv('{windows_style}')"),
        format!("select * from read_text('{outside}')"),
        format!("select * from glob('{folder}/../*')"),
        format!("attach '{other}' as other"),
        "install httpfs".into(),
        "load httpfs".into(),
        "select * from read_csv('https://example.com/data.csv')".into(),
        // The settings are locked, and changing them is not a read either.
        "set enable_external_access = true".into(),
        "reset allowed_directories".into(),
        format!("set allowed_directories = ['{folder}/../']"),
        "set lock_configuration = false".into(),
        "set temp_directory = 'x'".into(),
        // Writing to the folder is not a read.
        format!("copy (select 1 as a) to '{folder}/written.csv'"),
        format!("copy orders to '{folder}/written.parquet' (format parquet)"),
        format!("export database '{folder}/exported'"),
        "create table t (a int)".into(),
        "create or replace view orders as select 1".into(),
        "drop view orders".into(),
    ] {
        match query(&conn, &sql).await {
            Ok((_, rows)) => panic!("{sql} ran and returned {rows:?}"),
            Err(message) => assert!(!message.contains("leaked"), "{sql}: {message}"),
        }
    }
    // The views are still there and still read their files.
    let (_, rows) = query(&conn, "select count(*) from orders").await.unwrap();
    assert_eq!(rows, [["1"]]);
    assert_eq!(listing(&ws.path("data")), before, "nothing is written");
}

async fn plain_database(path: &Path) {
    engines::create_database(Engine::Duckdb, path).await;
}

#[tokio::test]
async fn a_folder_that_is_not_one_does_not_open() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("missing");
    ws.write("data/sales.csv", "id\n1\n");
    for (id, database) in [("file", "data/sales.csv"), ("missing", "data/nowhere")] {
        ws.connection(
            id,
            &format!("engine = \"duckdb\"\ndatabase = \"{database}\"\nfile_folder = true\n"),
        );
        match ws.open(id).await {
            Err(OpenError::FileFolder(message)) => {
                assert!(message.contains("is not a folder"), "{message}")
            }
            Err(other) => panic!("{database}: {other}"),
            Ok(_) => panic!("{database} opened"),
        }
    }
}

#[test]
fn file_folder_is_only_a_duckdb_folder_with_nothing_that_changes_its_session() {
    let _turn = crate::serial::file_turn(module_path!());
    for (toml, expected) in [
        (
            "engine = \"sqlite\"\ndatabase = \"data\"\nfile_folder = true\n",
            "file_folder is for DuckDB",
        ),
        (
            "engine = \"duckdb\"\ndatabase = \":memory:\"\nfile_folder = true\n",
            "not :memory:",
        ),
        (
            "engine = \"duckdb\"\ndatabase = \"data\"\nfile_folder = true\n[session]\nsetup = \"set threads = 1\"\n",
            "takes no [session] table",
        ),
        (
            "engine = \"duckdb\"\ndatabase = \"data\"\nfile_folder = true\n[session]\nschema = \"other\"\n",
            "takes no [session] table",
        ),
    ] {
        let ws = Workspace::new("config");
        ws.connection("sales", toml);
        let problems = ws.problems();
        assert!(
            problems.iter().any(|p| p.contains(expected)),
            "{toml}: {problems:?}"
        );
    }
}
