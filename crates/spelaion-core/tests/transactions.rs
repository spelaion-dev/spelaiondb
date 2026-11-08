//! Transactions you can see and control (ADR-031), against every engine: manual commit,
//! Commit and Rollback, smart commit, the state after statements that end a transaction on
//! their own, and a lost session with a transaction open. SQLite and DuckDB always run; the
//! servers need `SPELAION_TEST_PG`, `SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB` and
//! `SPELAION_TEST_MSSQL` (skipped without them, which does not count as tested).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    CoreEvent, EditError, ResultStore, Session, SessionHealth, TransactionError, TransactionState,
};
use spelaion_driver_api::{ConnectParams, Driver, ExecEvent, ExecOptions, Secret, TlsMode};
use spelaion_driver_duckdb::DuckdbDriver;
use spelaion_driver_mysql::MysqlDriver;
use spelaion_driver_postgres::PostgresDriver;
use spelaion_driver_sqlite::SqliteDriver;
use spelaion_driver_sqlserver::SqlServerDriver;

const SMART: Safeguards = Safeguards {
    confirm_execution: false,
    confirm_data_changes: true,
    close_idle_after: None,
    manual_commit: false,
    smart_commit: true,
};

/// Two sessions on the same database: `a` is the editor tab, `b` looks from outside.
struct Engine {
    name: &'static str,
    a: Session,
    b: Session,
    /// A user's `BEGIN` on this engine.
    begin: &'static str,
    /// What `b` adds to its `SELECT … FROM t` to not wait for `a`'s locks (SQL Server).
    no_wait: &'static str,
}

fn unique(prefix: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

async fn connect(driver: &dyn Driver, params: &ConnectParams) -> Session {
    Session::new(
        driver.connect(params).await.expect("connecting"),
        driver.dialect(),
        Arc::new(ResultStore::new()),
    )
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

async fn engines() -> Vec<Engine> {
    let mut out = Vec::new();
    let dir = std::env::temp_dir().join(unique("spelaion-tx"));
    std::fs::create_dir_all(&dir).unwrap();

    let sqlite = SqliteDriver {
        create_if_missing: true,
    };
    let file = ConnectParams {
        database: dir.join("tx.sqlite").display().to_string(),
        ..Default::default()
    };
    out.push(Engine {
        name: "sqlite",
        a: connect(&sqlite, &file).await,
        b: connect(&sqlite, &file).await,
        begin: "begin",
        no_wait: "",
    });

    let duckdb = DuckdbDriver::default();
    spelaion_driver_duckdb::create_database(&dir.join("tx.duckdb")).unwrap();
    let file = ConnectParams {
        database: dir.join("tx.duckdb").display().to_string(),
        ..Default::default()
    };
    out.push(Engine {
        name: "duckdb",
        a: connect(&duckdb, &file).await,
        b: connect(&duckdb, &file).await,
        begin: "begin transaction",
        no_wait: "",
    });

    if let Some(params) = server("SPELAION_TEST_PG", "spelaion", "spelaion") {
        out.push(Engine {
            name: "postgres",
            a: connect(&PostgresDriver, &params).await,
            b: connect(&PostgresDriver, &params).await,
            begin: "begin",
            no_wait: "",
        });
    }
    for (var, name) in [
        ("SPELAION_TEST_MYSQL", "mysql"),
        ("SPELAION_TEST_MARIADB", "mariadb"),
    ] {
        if let Some(params) = server(var, "spelaion", "spelaion") {
            let driver = if name == "mariadb" {
                MysqlDriver::mariadb()
            } else {
                MysqlDriver::mysql()
            };
            out.push(Engine {
                name,
                a: connect(&driver, &params).await,
                b: connect(&driver, &params).await,
                begin: "begin",
                no_wait: "",
            });
        }
    }
    if let Some(mut params) = server("SPELAION_TEST_MSSQL", "sa", "Spelaion-test-1") {
        params.tls = TlsMode::Require;
        params.database = "master".into();
        let master = connect(&SqlServerDriver, &params).await;
        run(
            &master,
            "if db_id('spelaion') is null create database spelaion",
        )
        .await
        .unwrap();
        params.database = "spelaion".into();
        out.push(Engine {
            name: "sqlserver",
            a: connect(&SqlServerDriver, &params).await,
            b: connect(&SqlServerDriver, &params).await,
            begin: "begin transaction",
            no_wait: " with (readpast)",
        });
    }
    out
}

/// Runs a script through the session (as the editor does); `Err` with the first error.
async fn run(session: &Session, sql: &str) -> Result<Vec<String>, String> {
    let mut notices = Vec::new();
    let mut events = session.run_script(sql);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => return Err(error.message),
            CoreEvent::Notice { notice, .. } => notices.push(notice.message),
            _ => {}
        }
    }
    Ok(notices)
}

/// The first value of a query, sent straight to the connection (no `BEGIN` before it).
async fn value(session: &Session, sql: &str) -> String {
    use spelaion_driver_api::{ColumnMeta, Value};
    let mut events = session
        .connection()
        .execute(sql, &ExecOptions::default())
        .await
        .expect(sql);
    let mut out = None;
    while let Some(event) = events.next().await {
        if let ExecEvent::Batch(batch) = event.expect(sql)
            && batch.num_rows() > 0
            && out.is_none()
        {
            let logical = ColumnMeta::from_field(batch.schema().field(0))
                .unwrap()
                .logical;
            out = Some(
                Value::from_array(batch.column(0).as_ref(), 0, logical)
                    .unwrap()
                    .to_string(),
            );
        }
    }
    out.unwrap_or_else(|| panic!("{sql}: no rows"))
}

async fn count(engine: &Engine, session: &Session, table: &str) -> String {
    value(
        session,
        &format!("select count(*) from {table}{}", engine.no_wait),
    )
    .await
}

#[tokio::test]
async fn manual_commit_commit_and_rollback_on_every_engine() {
    for engine in engines().await {
        let name = engine.name;
        let table = unique("tx_manual");
        run(
            &engine.a,
            &format!("create table {table} (id int primary key, v varchar(20))"),
        )
        .await
        .unwrap();

        let status = engine.a.follow_transactions(&Safeguards::default()).await;
        assert!(status.supported, "{name}");
        assert!(status.auto_commit, "{name}");
        assert_eq!(status.state, Some(TransactionState::Idle), "{name}");
        assert!(!engine.a.close_needs_confirmation().await, "{name}");
        assert_eq!(
            engine.a.commit().await.unwrap_err(),
            TransactionError::NotOpen,
            "{name}"
        );

        // Auto-commit off: several writes wait for Commit, and the badge says so.
        let status = engine.a.set_auto_commit(false).await.unwrap();
        assert!(!status.auto_commit, "{name}");
        run(
            &engine.a,
            &format!("insert into {table} values (1, 'a'); insert into {table} values (2, 'b')"),
        )
        .await
        .unwrap();
        let status = engine.a.transaction_status();
        assert_eq!(status.state, Some(TransactionState::Open), "{name}");
        assert_eq!(status.statements, 2, "{name}");
        assert_eq!(count(&engine, &engine.b, &table).await, "0", "{name}");
        assert!(engine.a.close_needs_confirmation().await, "{name}");
        assert_eq!(
            engine.a.set_auto_commit(true).await.unwrap_err(),
            TransactionError::Open,
            "{name}"
        );

        let outcome = engine.a.rollback().await.unwrap();
        assert_eq!(outcome.status.state, Some(TransactionState::Idle), "{name}");
        assert!(!outcome.status.auto_commit, "{name}: manual stays manual");
        assert_eq!(count(&engine, &engine.a, &table).await, "0", "{name}");

        run(&engine.a, &format!("insert into {table} values (3, 'c')"))
            .await
            .unwrap();
        assert!(engine.a.transaction_status().is_open(), "{name}");
        assert_eq!(count(&engine, &engine.b, &table).await, "0", "{name}");
        let outcome = engine.a.commit().await.unwrap();
        assert_eq!(outcome.status.state, Some(TransactionState::Idle), "{name}");
        assert_eq!(count(&engine, &engine.b, &table).await, "1", "{name}");
        assert!(!engine.a.close_needs_confirmation().await, "{name}");

        // A read opens a transaction too in manual commit (a function may write).
        run(&engine.a, &format!("select count(*) from {table}"))
            .await
            .unwrap();
        assert!(engine.a.transaction_status().is_open(), "{name}");
        engine.a.rollback().await.unwrap();

        // Back to auto-commit: each statement commits, and a user's BEGIN shows.
        assert!(engine.a.set_auto_commit(true).await.unwrap().auto_commit);
        run(&engine.a, &format!("insert into {table} values (4, 'd')"))
            .await
            .unwrap();
        assert_eq!(
            engine.a.transaction_status().state,
            Some(TransactionState::Idle),
            "{name}"
        );
        assert_eq!(count(&engine, &engine.b, &table).await, "2", "{name}");
        run(&engine.a, engine.begin).await.unwrap();
        run(&engine.a, &format!("delete from {table} where id = 4"))
            .await
            .unwrap();
        let status = engine.a.transaction_status();
        assert!(status.is_open(), "{name}: a typed BEGIN shows");
        assert_eq!(status.statements, 1, "{name}");
        run(&engine.a, "rollback").await.unwrap();
        assert_eq!(
            engine.a.transaction_status().state,
            Some(TransactionState::Idle),
            "{name}"
        );
        assert_eq!(count(&engine, &engine.b, &table).await, "2", "{name}");

        run(&engine.b, &format!("drop table {table}"))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn smart_commit_switches_to_manual_on_the_first_write() {
    for engine in engines().await {
        let name = engine.name;
        let table = unique("tx_smart");
        run(
            &engine.a,
            &format!(
                "create table {table} (id int primary key, v varchar(20)); insert into {table} values (1, 'old')"
            ),
        )
        .await
        .unwrap();
        let status = engine.a.follow_transactions(&SMART).await;
        assert!(status.auto_commit && status.smart_commit, "{name}");

        // Reads stay in auto-commit, with no transaction left open.
        run(&engine.a, &format!("select v from {table}"))
            .await
            .unwrap();
        let status = engine.a.transaction_status();
        assert!(status.auto_commit && !status.smart_switched, "{name}");
        assert_eq!(status.state, Some(TransactionState::Idle), "{name}");

        // The first write switches to manual, inside a transaction.
        run(
            &engine.a,
            &format!("update {table} set v = 'new' where id = 1"),
        )
        .await
        .unwrap();
        let status = engine.a.transaction_status();
        assert!(!status.auto_commit && status.smart_switched, "{name}");
        assert!(status.is_open(), "{name}");
        // The user cannot loosen it: auto-commit back on asks for Commit or Rollback first.
        assert_eq!(
            engine.a.set_auto_commit(true).await.unwrap_err(),
            TransactionError::Open
        );
        let outcome = engine.a.rollback().await.unwrap();
        assert!(outcome.status.auto_commit, "{name}: back to auto-commit");
        assert!(!outcome.status.smart_switched, "{name}");
        assert_eq!(
            value(&engine.b, &format!("select v from {table}")).await,
            "old",
            "{name}"
        );

        run(
            &engine.a,
            &format!("update {table} set v = 'new' where id = 1"),
        )
        .await
        .unwrap();
        assert!(engine.a.transaction_status().smart_switched, "{name}");
        let outcome = engine.a.commit().await.unwrap();
        assert!(outcome.status.auto_commit, "{name}");
        assert_eq!(
            value(&engine.b, &format!("select v from {table}")).await,
            "new",
            "{name}"
        );

        // The type's manual commit starts tabs with auto-commit off.
        let status = engine
            .a
            .follow_transactions(&Safeguards {
                manual_commit: true,
                ..SMART
            })
            .await;
        assert!(!status.auto_commit && !status.smart_switched, "{name}");

        run(&engine.b, &format!("drop table {table}"))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn schema_metadata_keeps_user_transaction_counts_on_every_engine() {
    for engine in engines().await {
        let name = engine.name;
        let table = unique("tx_metadata");
        run(&engine.a, &format!("create table {table} (id int)"))
            .await
            .unwrap();
        engine.a.follow_transactions(&SMART).await;
        let schema = spelaion_core::active_schema(&engine.a).await.unwrap();
        assert_eq!(engine.a.transaction_status().statements, 0, "{name}");
        assert_eq!(
            engine.a.transaction_status().state,
            Some(TransactionState::Idle),
            "{name}: metadata does not start a transaction"
        );

        run(&engine.a, &format!("insert into {table} values (1)"))
            .await
            .unwrap();
        let results_before = engine.a.results().len();
        for _ in 0..3 {
            assert_eq!(
                spelaion_core::active_schema(&engine.a).await.unwrap(),
                schema,
                "{name}"
            );
            let status = engine.a.transaction_status();
            assert!(status.is_open() && status.smart_switched, "{name}");
            assert_eq!(status.statements, 1, "{name}: metadata is not user work");
            assert_eq!(engine.a.results().len(), results_before, "{name}");
        }

        // An explicit generated action still counts, even though its SQL is core-written.
        assert_eq!(
            spelaion_core::count_rows(&engine.a, schema.as_deref().unwrap_or(""), &table)
                .await
                .unwrap(),
            1,
            "{name}"
        );
        assert_eq!(engine.a.transaction_status().statements, 2, "{name}");
        engine.a.rollback().await.unwrap();
        assert_eq!(count(&engine, &engine.b, &table).await, "0", "{name}");

        engine.a.set_auto_commit(false).await.unwrap();
        spelaion_core::active_schema(&engine.a).await.unwrap();
        assert_eq!(
            engine.a.transaction_status().state,
            Some(TransactionState::Idle),
            "{name}: metadata does not send manual-mode BEGIN"
        );
        assert_eq!(engine.a.transaction_status().statements, 0, "{name}");
        run(&engine.b, &format!("drop table {table}"))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn postgres_smart_commit_schema_refresh_does_not_add_a_second_statement() {
    let Some(params) = server("SPELAION_TEST_PG", "spelaion", "spelaion") else {
        return;
    };
    let session = connect(&PostgresDriver, &params).await;
    session.follow_transactions(&SMART).await;
    run(&session, "create temp table smart_metadata (id int)")
        .await
        .unwrap();
    assert_eq!(session.transaction_status().statements, 1);
    for _ in 0..3 {
        assert_eq!(
            spelaion_core::active_schema(&session)
                .await
                .unwrap()
                .as_deref(),
            Some("public")
        );
        assert_eq!(session.transaction_status().statements, 1);
    }
    // Setting the schema is a user's action, unlike reading the current one.
    spelaion_core::set_active_schema(&session, "public")
        .await
        .unwrap();
    assert_eq!(session.transaction_status().statements, 2);
    spelaion_core::active_schema(&session).await.unwrap();
    assert_eq!(session.transaction_status().statements, 2);
    assert!(session.rollback().await.unwrap().status.auto_commit);
}

#[tokio::test]
async fn statements_that_end_a_transaction_keep_the_badge_true() {
    for engine in engines().await {
        let name = engine.name;
        let table = unique("tx_end");
        run(
            &engine.a,
            &format!("create table {table} (id int primary key)"),
        )
        .await
        .unwrap();
        engine.a.follow_transactions(&Safeguards::default()).await;
        engine.a.set_auto_commit(false).await.unwrap();
        run(&engine.a, &format!("insert into {table} values (1)"))
            .await
            .unwrap();
        assert!(engine.a.transaction_status().is_open(), "{name}");

        match name {
            // DDL commits implicitly: the badge goes, and the run says so.
            "mysql" | "mariadb" => {
                let notices = run(&engine.a, &format!("create table {table}_2 (id int)"))
                    .await
                    .unwrap();
                assert!(
                    notices
                        .iter()
                        .any(|n| n.contains("committed by this statement")),
                    "{name}: {notices:?}"
                );
                let status = engine.a.transaction_status();
                assert_eq!(status.state, Some(TransactionState::Idle), "{name}");
                assert_eq!(count(&engine, &engine.b, &table).await, "1", "{name}");
                // A new BEGIN commits the open transaction too.
                run(&engine.a, &format!("insert into {table} values (2)"))
                    .await
                    .unwrap();
                let notices = run(&engine.a, "begin").await.unwrap();
                assert!(
                    notices
                        .iter()
                        .any(|n| n.contains("committed by this statement")),
                    "{name}: {notices:?}"
                );
                assert_eq!(count(&engine, &engine.b, &table).await, "2", "{name}");
                engine.a.rollback().await.unwrap();
                run(&engine.b, &format!("drop table {table}_2"))
                    .await
                    .unwrap();
            }
            // An error leaves a transaction that can only be rolled back.
            "postgres" | "duckdb" => {
                assert!(
                    run(&engine.a, &format!("insert into {table} values (1)"))
                        .await
                        .is_err()
                );
                let status = engine.a.transaction_status();
                assert_eq!(status.state, Some(TransactionState::Failed), "{name}");
                // Nothing to lose by closing it, but Commit can only roll it back.
                assert!(!engine.a.close_needs_confirmation().await, "{name}");
                let outcome = engine.a.commit().await.unwrap();
                assert_eq!(outcome.status.state, Some(TransactionState::Idle), "{name}");
                assert!(
                    outcome
                        .notices
                        .iter()
                        .any(|n| n.message.contains("rolled it back")),
                    "{name}: {:?}",
                    outcome.notices
                );
                assert_eq!(count(&engine, &engine.b, &table).await, "0", "{name}");
            }
            // With XACT_ABORT an error rolls the whole transaction back.
            "sqlserver" => {
                run(&engine.a, "set xact_abort on").await.unwrap();
                let error = run(&engine.a, &format!("insert into {table} values (1)"))
                    .await
                    .unwrap_err();
                assert!(
                    error.contains("PRIMARY KEY") || error.contains("duplicate"),
                    "{error}"
                );
                let status = engine.a.transaction_status();
                assert_eq!(status.state, Some(TransactionState::Idle), "{name}");
                assert_eq!(count(&engine, &engine.b, &table).await, "0", "{name}");
                run(&engine.a, "set xact_abort off").await.unwrap();
                engine.a.rollback().await.unwrap();
            }
            // SQLite has no failed state: a failed statement leaves the transaction open.
            "sqlite" => {
                assert!(
                    run(&engine.a, &format!("insert into {table} values (1)"))
                        .await
                        .is_err()
                );
                assert!(engine.a.transaction_status().is_open(), "{name}");
                engine.a.rollback().await.unwrap();
            }
            other => panic!("no case for {other}"),
        }
        run(&engine.b, &format!("drop table {table}"))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn myisam_rollback_says_its_changes_stayed() {
    for engine in engines().await {
        if !matches!(engine.name, "mysql" | "mariadb") {
            continue;
        }
        let table = unique("tx_myisam");
        run(
            &engine.a,
            &format!("create table {table} (id int primary key) engine = MyISAM"),
        )
        .await
        .unwrap();
        engine.a.follow_transactions(&Safeguards::default()).await;
        engine.a.set_auto_commit(false).await.unwrap();
        run(&engine.a, &format!("insert into {table} values (1)"))
            .await
            .unwrap();
        let outcome = engine.a.rollback().await.unwrap();
        assert!(
            outcome
                .notices
                .iter()
                .any(|n| n.message.contains("non-transactional")),
            "{}: {:?}",
            engine.name,
            outcome.notices
        );
        assert_eq!(count(&engine, &engine.b, &table).await, "1");
        run(&engine.b, &format!("drop table {table}"))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_lost_session_remembers_its_open_transaction() {
    for engine in engines().await {
        let kill = match engine.name {
            "postgres" => "select pg_terminate_backend({id})",
            "mysql" | "mariadb" => "kill {id}",
            "sqlserver" => "kill {id}",
            // Files cannot be lost.
            _ => continue,
        };
        let own_id = match engine.name {
            "postgres" => "select pg_backend_pid()",
            "sqlserver" => "select @@spid",
            _ => "select connection_id()",
        };
        let name = engine.name;
        let table = unique("tx_lost");
        run(
            &engine.a,
            &format!("create table {table} (id int primary key)"),
        )
        .await
        .unwrap();
        engine.a.follow_transactions(&Safeguards::default()).await;
        engine.a.set_auto_commit(false).await.unwrap();
        run(&engine.a, &format!("insert into {table} values (1)"))
            .await
            .unwrap();
        let id = value(&engine.a, own_id).await;
        // `sa` may kill; the PostgreSQL and MySQL test users end their own sessions.
        run(&engine.b, &kill.replace("{id}", &id)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(run(&engine.a, "select 1").await.is_err(), "{name}");
        assert!(
            matches!(*engine.a.health().borrow(), SessionHealth::Lost { .. }),
            "{name}"
        );
        // The tab can say that a transaction was open when it was lost; nothing reconnects.
        assert!(engine.a.transaction_status().is_open(), "{name}");
        assert_eq!(count(&engine, &engine.b, &table).await, "0", "{name}");
        run(&engine.b, &format!("drop table {table}"))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn table_edits_refuse_a_session_with_an_open_transaction() {
    let dir = std::env::temp_dir().join(unique("spelaion-tx-edit"));
    std::fs::create_dir_all(&dir).unwrap();
    let driver = SqliteDriver {
        create_if_missing: true,
    };
    let session = connect(
        &driver,
        &ConnectParams {
            database: dir.join("edit.sqlite").display().to_string(),
            ..Default::default()
        },
    )
    .await;
    run(&session, "create table t (id integer primary key, v text)")
        .await
        .unwrap();
    run(&session, "begin; insert into t values (1, 'a')")
        .await
        .unwrap();
    let edit = vec!["update t set v = 'b' where id = 1".to_string()];
    assert!(matches!(
        spelaion_core::apply_edits(&session, &edit).await,
        Err(EditError::TransactionOpen)
    ));
    run(&session, "commit").await.unwrap();
    assert_eq!(spelaion_core::apply_edits(&session, &edit).await, Ok(1));
}

#[tokio::test]
async fn sessions_that_do_not_follow_transactions_run_as_before() {
    // Agents' sessions and table data tabs never call `follow_transactions`.
    let driver = SqliteDriver::default();
    let session = connect(
        &driver,
        &ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        },
    )
    .await;
    assert!(!session.transaction_status().supported);
    assert_eq!(
        session.set_auto_commit(false).await.unwrap_err(),
        TransactionError::Unsupported
    );
    run(&session, "create table t (a int); insert into t values (1)")
        .await
        .unwrap();
    assert_eq!(session.transaction_status().state, None);
    assert!(!session.close_needs_confirmation().await);
}
