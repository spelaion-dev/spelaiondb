//! SQL notebooks (ADR-087) against real engines: a notebook whose cells reference each other is
//! written to a file, read back, expanded and run on each engine, the way the app runs a cell
//! (`check_run`, then a script run whose results the cell's own result tabs keep). The guard
//! sees the expanded statement: a read-only session runs a cell whose references are reads,
//! and a reference to a write is refused before anything runs. SQLite and DuckDB always run;
//! the servers need `SPELAION_TEST_PG`, `SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB` and
//! `SPELAION_TEST_MSSQL` (skipped without them, which does not count as tested).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::notebook::references::{ReferenceError, expand_cell};
use spelaion_core::notebook::{Cell, Notebook, read_notebook_file, write_notebook_file};

use spelaion_core::{
    CoreEvent, FileFormat, Placement, ResultStore, ResultTabs, SaveCheck, Session, check_read_only,
    check_run,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

use crate::engines;

fn unique(prefix: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

struct Engine {
    name: &'static str,
    session: Session,
    store: Arc<ResultStore>,
}

async fn connect(name: &'static str, driver: &dyn Driver, params: &ConnectParams) -> Engine {
    let store = Arc::new(ResultStore::new());
    let session = Session::new(
        driver.connect(params).await.expect("connecting"),
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        store.clone(),
    );
    Engine {
        name,
        session,
        store,
    }
}

fn server(var: &str, user: &str, password: &str) -> Option<ConnectParams> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: user.into(),
        password: Some(Secret::new(password)),
        database: "spelaion".into(),
        ..Default::default()
    })
}

async fn engines(dir: &std::path::Path) -> Vec<Engine> {
    let mut out = Vec::new();
    engines::create_database(spelaion_config::Engine::Sqlite, &dir.join("nb.sqlite")).await;
    let file = ConnectParams {
        database: dir.join("nb.sqlite").display().to_string(),
        ..Default::default()
    };
    out.push(connect("sqlite", &*engines::sqlite(), &file).await);
    engines::create_database(spelaion_config::Engine::Duckdb, &dir.join("nb.duckdb")).await;
    let file = ConnectParams {
        database: dir.join("nb.duckdb").display().to_string(),
        ..Default::default()
    };
    out.push(connect("duckdb", &*engines::duckdb(), &file).await);
    if let Some(params) = server("SPELAION_TEST_PG", "spelaion", "spelaion") {
        out.push(connect("postgres", &*engines::postgres(), &params).await);
    }
    if let Some(params) = server("SPELAION_TEST_MYSQL", "spelaion", "spelaion") {
        out.push(connect("mysql", &*engines::mysql(), &params).await);
    }
    if let Some(params) = server("SPELAION_TEST_MARIADB", "spelaion", "spelaion") {
        out.push(connect("mariadb", &*engines::mariadb(), &params).await);
    }
    if let Some(mut params) = server("SPELAION_TEST_MSSQL", "sa", "Spelaion-test-1") {
        params.tls = TlsMode::Require;
        params.database = "master".into();
        let master = connect("sqlserver", &*engines::sqlserver(), &params).await;
        master
            .run("if db_id('spelaion') is null create database spelaion")
            .await
            .expect("creating the database");
        params.database = "spelaion".into();
        out.push(connect("sqlserver", &*engines::sqlserver(), &params).await);
    }
    out
}

impl Engine {
    /// Runs `script` as the app runs a cell: the guard first, then a script run whose results
    /// go to the cell's own result tabs. Returns each result's rows as text, or the first error.
    async fn run_cell(
        &self,
        tabs: &Arc<ResultTabs>,
        cell: &str,
        script: &str,
    ) -> Result<Vec<Vec<Vec<Option<String>>>>, String> {
        self.run_confirmed(tabs, cell, script, false).await
    }

    async fn run_confirmed(
        &self,
        tabs: &Arc<ResultTabs>,
        cell: &str,
        script: &str,
        confirmed: bool,
    ) -> Result<Vec<Vec<Vec<Option<String>>>>, String> {
        check_run(&self.session, script, &Safeguards::default(), confirmed)
            .map_err(|e| e.to_string())?;
        let mut placed = tabs
            .begin_run(
                cell,
                Placement::Replace,
                self.session.dialect(),
                script,
                None,
                20,
            )
            .map_err(|e| e.to_string())?;
        let mut events = self.session.run_script(script);
        let mut error = None;
        while let Some(event) = events.next().await {
            if let CoreEvent::StatementFailed { error: failed, .. } = &event {
                error.get_or_insert_with(|| failed.message.clone());
            }
            placed.observe(&event);
        }
        if let Some(error) = error {
            return Err(error);
        }
        Ok(tabs
            .list(cell)
            .into_iter()
            .map(|tab| {
                let stored = self.store.get(tab.result).expect("a kept result");
                let stored = stored.read().unwrap();
                let columns = stored.schema().columns.len();
                (0..stored.rows())
                    .map(|row| {
                        (0..columns)
                            .map(|c| match stored.value_at(row, c) {
                                None | Some(Value::Null) => None,
                                Some(v) => Some(v.to_string()),
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect())
    }

    async fn run(&self, script: &str) -> Result<(), String> {
        let tabs = Arc::new(ResultTabs::new(self.store.clone()));
        // The test's own setup and cleanup, confirmed as the user would.
        self.run_confirmed(&tabs, "setup", script, true)
            .await
            .map(|_| ())
    }
}

fn sql(name: &str, sql: &str) -> Cell {
    Cell::Sql {
        name: name.into(),
        sql: sql.into(),
    }
}

/// A notebook over table `t`: `paid` reads it, `per_region` groups `paid`, and `report` reads
/// `per_region` and `paid` again.
fn notebook(t: &str) -> Notebook {
    Notebook {
        connection: Some("~/local".into()),
        cells: vec![
            Cell::Markdown {
                text: "# Paid orders\n\nPer region, then **the biggest**.".into(),
            },
            sql(
                "paid",
                &format!("SELECT id, region, amount FROM {t} WHERE paid = 1;"),
            ),
            sql(
                "per_region",
                "-- one row per region\nSELECT region, SUM(amount) AS total FROM {{paid}} GROUP BY region",
            ),
            sql(
                "report",
                "SELECT r.region, r.total, COUNT(*) AS orders\nFROM {{per_region}} r\nJOIN {{ paid }} p ON p.region = r.region\nGROUP BY r.region, r.total\nORDER BY r.total DESC",
            ),
            sql("change", &format!("DELETE FROM {t} WHERE id = 1")),
            sql("uses_change", "SELECT * FROM {{change}}"),
        ],
    }
}

#[tokio::test]
async fn notebook_cells_reference_each_other_on_every_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    let dir = std::env::temp_dir().join(unique("spelaion-notebooks"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for engine in engines(&dir).await {
        let t = unique("nb_orders");
        engine
            .run(&format!(
                "CREATE TABLE {t} (id int, region varchar(20), amount int, paid int)"
            ))
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", engine.name));
        engine
            .run(&format!(
                "INSERT INTO {t} VALUES (1, 'north', 10, 1), (2, 'north', 5, 1), (3, 'south', 30, 1), (4, 'south', 99, 0)"
            ))
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", engine.name));

        // Saved and read back before it runs: a file round trip, and nothing ran.
        let path = dir.join(format!("{t}.md"));
        write_notebook_file(
            &path,
            &notebook(&t),
            FileFormat::new_file(),
            &SaveCheck::Overwrite,
        )
        .unwrap();
        let read = read_notebook_file(&path).unwrap().notebook;
        assert_eq!(read, notebook(&t));

        let dialect = engine.session.dialect();
        let tabs = Arc::new(ResultTabs::new(engine.store.clone()));
        let expanded = expand_cell(&read, 3, dialect.as_ref()).unwrap();
        assert_eq!(
            expanded.references,
            ["paid", "per_region"],
            "{}",
            engine.name
        );
        let rows = engine
            .run_cell(&tabs, "report", &expanded.sql)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}\n{}", engine.name, expanded.sql));
        assert_eq!(
            rows,
            vec![vec![
                vec![Some("south".into()), Some("30".into()), Some("1".into())],
                vec![Some("north".into()), Some("15".into()), Some("2".into())],
            ]],
            "{}: {}",
            engine.name,
            expanded.sql
        );

        // A cell runs again into its own result tabs: the previous result is replaced.
        let again = engine
            .run_cell(&tabs, "report", &expanded.sql)
            .await
            .unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(tabs.list("report").len(), 1);

        // Referencing a write is refused before anything runs: the row is still there.
        assert!(matches!(
            expand_cell(&read, 5, dialect.as_ref()),
            Err(ReferenceError::NotARead { .. })
        ));
        let left = engine
            .run_cell(
                &tabs,
                "count",
                &format!("SELECT COUNT(*) FROM {t} WHERE id = 1"),
            )
            .await
            .unwrap();
        assert_eq!(left, vec![vec![vec![Some("1".into())]]], "{}", engine.name);

        // The read-only barrier (ADR-053) reads the expanded statement: a cell whose
        // references are reads passes it, and the cell that writes does not.
        assert!(
            check_read_only(dialect.as_ref(), &expanded.sql).is_ok(),
            "{}: {}",
            engine.name,
            expanded.sql
        );
        let write = expand_cell(&read, 4, dialect.as_ref()).unwrap();
        assert!(
            check_read_only(dialect.as_ref(), &write.sql).is_err(),
            "{}",
            engine.name
        );
        let _ = engine.run(&format!("DROP TABLE {t}")).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
}
