//! A copy whose source and target are one SQLite file (ADR-069). The copy reads the source on
//! one session while it writes the target on another, and on one file the read's lock keeps
//! the writes from committing ("database is locked"). So it is refused before anything runs,
//! with a reason that says what to do instead, whichever connections name the file and however
//! they spell its path. Another file still copies. SQLite needs nothing external.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::table_copy::{CopyError, CopyHow, CopyRequest, plan_copy, run_copy};
use spelaion_core::{
    CoreEvent, Engines, IfExists, JobCancel, OpenConnection, OpenOptions, ResultStore, TunnelPool,
    open_connection,
};
use spelaion_driver_api::Value;
use spelaion_ssh::{KnownHosts, NonInteractive};

const NONE: Safeguards = Safeguards {
    confirm_execution: false,
    confirm_data_changes: false,
    close_idle_after: None,
    manual_commit: false,
    smart_commit: false,
};

struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-copy-same-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion/connections")).unwrap();
        std::fs::create_dir_all(base.join("repo/data")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn sqlite(&self, id: &str, database: &str) {
        std::fs::write(
            self.base
                .join(format!("repo/.spelaion/connections/{id}.toml")),
            format!("schema_version = 1\nengine = \"sqlite\"\ndatabase = \"{database}\"\n"),
        )
        .unwrap();
    }

    /// Opened as the app opens a copy's sessions: the source as a session that only reads.
    async fn open(&self, id: &str, read_only_session: bool) -> OpenConnection {
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
                read_only_session,
            },
            Arc::new(ResultStore::new()),
        )
        .await
        .expect("opening")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

async fn exec(conn: &OpenConnection, script: &str) {
    let events: Vec<CoreEvent> = conn.session.run_script(script).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{script}: {error:?}");
    }
}

async fn value(conn: &OpenConnection, sql: &str) -> String {
    let events: Vec<CoreEvent> = conn.session.run_script(sql).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{sql}: {error:?}");
    }
    let result = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("no result");
    let stored = conn.session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    Value::from_array(
        batch.column(0).as_ref(),
        0,
        stored.schema().columns[0].logical,
    )
    .unwrap()
    .to_string()
}

fn request(target_table: &str) -> CopyRequest {
    CopyRequest {
        source_schema: "main".into(),
        source_table: "orders".into(),
        target_schema: "main".into(),
        target_table: target_table.into(),
        how: CopyHow::NewTable {
            if_exists: IfExists::Fail,
            rows: true,
        },
        skip: Vec::new(),
        types: Vec::new(),
        commit_every: 0,
    }
}

#[tokio::test]
async fn a_copy_into_the_same_sqlite_file_is_refused_before_it_runs_and_another_file_copies() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new();
    std::fs::write(ws.base.join("repo/data/shop.db"), "").unwrap();
    std::fs::write(ws.base.join("repo/data/other.db"), "").unwrap();
    ws.sqlite("shop", "data/shop.db");
    // The same file under another connection and another spelling of its path.
    ws.sqlite("shop_again", "data/../data/shop.db");
    ws.sqlite("other", "data/other.db");

    let setup = ws.open("shop", false).await;
    exec(
        &setup,
        "create table orders (id integer primary key, name text not null);\n\
         insert into orders values (1, 'one'), (2, 'two'), (3, 'three');",
    )
    .await;
    setup.close().await;

    let source = ws.open("shop", true).await;
    let same = ws.open("shop", false).await;
    let again = ws.open("shop_again", false).await;
    let other = ws.open("other", false).await;
    let shop_file = source
        .session
        .database_file()
        .expect("a SQLite file is named");
    assert_eq!(same.session.database_file(), Some(shop_file));
    assert_eq!(again.session.database_file(), Some(shop_file));
    assert_ne!(other.session.database_file(), Some(shop_file));

    for (label, target) in [
        ("the same connection", &same),
        ("another connection", &again),
    ] {
        // Refused when planned: the dialog says so in place of the columns.
        let planned = plan_copy(
            &source.session,
            &target.session,
            &request("orders_copy"),
            &NONE,
            false,
        )
        .await;
        assert!(
            matches!(planned, Err(CopyError::SameFile)),
            "{label}: {:?}",
            planned.map(|p| p.target.statements)
        );
        // And when run, before anything runs: no lock is waited on, no table is created.
        let ran = run_copy(
            &source.session,
            &target.session,
            &request("orders_copy"),
            &NONE,
            false,
            false,
            &JobCancel::new(),
            &|_| {},
        )
        .await;
        let reason = match ran {
            Err(e @ CopyError::SameFile) => e.to_string(),
            other => panic!("{label}: {:?}", other.map(|s| s.rows)),
        };
        assert!(reason.contains("same database file"), "{reason}");
        assert!(reason.contains("CREATE TABLE … AS SELECT"), "{reason}");
    }
    assert_eq!(
        value(
            &same,
            "select count(*) from sqlite_master where name = 'orders_copy'"
        )
        .await,
        "0",
        "nothing was created on the shared file"
    );

    // Another file copies, with the source still open on its own.
    let summary = run_copy(
        &source.session,
        &other.session,
        &request("orders_copy"),
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &|_| {},
    )
    .await
    .expect("copying to another file");
    assert_eq!(summary.rows, 3);
    assert_eq!(
        value(&other, "select group_concat(name, ',') from orders_copy").await,
        "one,two,three"
    );

    for conn in [source, same, again, other] {
        conn.close().await;
    }
}
