//! Script runs that behave like a script runner (ADR-035), against every engine: a script with
//! an error in statement 3 of 5 in each error mode, in auto-commit and in manual commit; the
//! "ask" mode's tickets; and statement timeouts. SQLite and DuckDB always run; the servers
//! need `SPELAION_TEST_PG`, `SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB` and
//! `SPELAION_TEST_MSSQL` (skipped without them, which does not count as tested).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    AnswerError, CoreEvent, ErrorAnswer, ErrorTicket, OnError, ResultStore, ScriptOptions,
    ScriptStop, ScriptSummary, Session, TransactionState,
};
use spelaion_driver_api::{ConnectParams, Driver, ExecEvent, ExecOptions, Secret, TlsMode};

mod engines;
mod support;
use support::{CLEANUP_TIMEOUT, cleaning_up, run_cleanup};

/// Two sessions on the same database: `a` is the editor tab, `b` looks from outside.
struct Engine {
    name: &'static str,
    a: Session,
    b: Session,
    /// What `b` adds to its `SELECT … FROM t` to not wait for `a`'s locks (SQL Server).
    no_wait: &'static str,
}

impl Engine {
    /// A failed statement leaves the transaction it ran in aborted: only a rollback is left.
    fn aborts_transactions(&self) -> bool {
        matches!(self.name, "postgres" | "duckdb")
    }
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
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
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
    let dir = std::env::temp_dir().join(unique("spelaion-script"));
    // What an earlier run left behind: the system gives a process id again.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    engines::create_database(spelaion_config::Engine::Sqlite, &dir.join("runs.sqlite")).await;
    let sqlite = engines::sqlite();
    let file = ConnectParams {
        database: dir.join("runs.sqlite").display().to_string(),
        ..Default::default()
    };
    out.push(Engine {
        name: "sqlite",
        a: connect(&sqlite, &file).await,
        b: connect(&sqlite, &file).await,
        no_wait: "",
    });

    let duckdb = engines::duckdb();
    engines::create_database(spelaion_config::Engine::Duckdb, &dir.join("runs.duckdb")).await;
    let file = ConnectParams {
        database: dir.join("runs.duckdb").display().to_string(),
        ..Default::default()
    };
    out.push(Engine {
        name: "duckdb",
        a: connect(&duckdb, &file).await,
        b: connect(&duckdb, &file).await,
        no_wait: "",
    });

    if let Some(params) = server("SPELAION_TEST_PG", "spelaion", "spelaion") {
        out.push(Engine {
            name: "postgres",
            a: connect(&*engines::postgres(), &params).await,
            b: connect(&*engines::postgres(), &params).await,
            no_wait: "",
        });
    }
    for (var, name) in [
        ("SPELAION_TEST_MYSQL", "mysql"),
        ("SPELAION_TEST_MARIADB", "mariadb"),
    ] {
        if let Some(params) = server(var, "spelaion", "spelaion") {
            let driver = if name == "mariadb" {
                engines::mariadb()
            } else {
                engines::mysql()
            };
            out.push(Engine {
                name,
                a: connect(&driver, &params).await,
                b: connect(&driver, &params).await,
                no_wait: "",
            });
        }
    }
    if let Some(mut params) = server("SPELAION_TEST_MSSQL", "sa", "Spelaion-test-1") {
        params.tls = TlsMode::Require;
        params.database = "master".into();
        let master = connect(&*engines::sqlserver(), &params).await;
        run(
            &master,
            "if db_id('spelaion') is null create database spelaion",
            ScriptOptions::default(),
            &[],
        )
        .await;
        params.database = "spelaion".into();
        out.push(Engine {
            name: "sqlserver",
            a: connect(&*engines::sqlserver(), &params).await,
            b: connect(&*engines::sqlserver(), &params).await,
            no_wait: " with (readpast)",
        });
    }
    out
}

/// What a run did, from its events.
#[derive(Debug, Default)]
struct Outcome {
    finished: Vec<usize>,
    failed: Vec<usize>,
    timed_out: Vec<usize>,
    /// `(index, remaining)` of every pause.
    paused: Vec<(usize, usize)>,
    notices: Vec<String>,
    summary: ScriptSummary,
    /// Per finished statement: execution + fetch never exceed the statement's time.
    timing_ok: bool,
}

/// Runs a script through the session as the editor does, answering each pause with the next
/// of `answers`.
async fn run(
    session: &Session,
    sql: &str,
    options: ScriptOptions,
    answers: &[ErrorAnswer],
) -> Outcome {
    let mut out = Outcome {
        timing_ok: true,
        ..Default::default()
    };
    let mut answers = answers.iter();
    let mut events = session.run_script_with(sql, options);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::StatementFinished {
                index,
                elapsed,
                execution,
                fetch,
                ..
            } => {
                out.finished.push(index);
                out.timing_ok &= execution + fetch <= elapsed;
            }
            CoreEvent::StatementFailed { index, error } => {
                out.failed.push(index);
                if error.timed_out {
                    out.timed_out.push(index);
                }
            }
            CoreEvent::Notice { notice, .. } => {
                out.notices
                    .push(format!("{}: {}", notice.severity, notice.message));
            }
            CoreEvent::ErrorPaused {
                index,
                ticket,
                remaining,
            } => {
                out.paused.push((index, remaining));
                let answer = *answers.next().expect("an answer for every pause");
                session.answer_error(ticket, answer).unwrap();
            }
            CoreEvent::ScriptFinished { summary, .. } => out.summary = summary,
            _ => {}
        }
    }
    out
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

/// A test's cleanup: drops its tables from `b`. When the test failed, it first stops a run
/// it left paused and rolls back what it may have left open in `a`, which would hold the
/// tables.
async fn drop_tables(engine: &Engine, tables: Vec<String>, panicked: bool) {
    if panicked {
        let _ = engine.a.stop_paused_run();
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, engine.a.rollback()).await;
    }
    let drops: Vec<String> = tables
        .iter()
        .map(|table| format!("drop table if exists {table}"))
        .collect();
    run_cleanup(engine.name, &engine.b, &drops, panicked).await;
}

async fn count(engine: &Engine, table: &str) -> String {
    value(
        &engine.b,
        &format!("select count(*) from {table}{}", engine.no_wait),
    )
    .await
}

fn options(on_error: OnError) -> ScriptOptions {
    ScriptOptions {
        on_error,
        statement_timeout: None,
    }
}

/// Five inserts; the third repeats the first's key and fails.
fn three_of_five(table: &str) -> String {
    [1, 2, 1, 3, 4]
        .map(|id| format!("insert into {table} values ({id})"))
        .join(";\n")
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Stop,
    Continue,
    AskSkip,
    AskStop,
}

#[tokio::test]
async fn an_error_in_statement_3_of_5_in_every_mode_on_every_engine() {
    for engine in engines().await {
        let name = engine.name;
        engine.a.follow_transactions(&Safeguards::default()).await;
        for manual in [false, true] {
            for mode in [Mode::Stop, Mode::Continue, Mode::AskSkip, Mode::AskStop] {
                let case = format!("{name}, manual commit {manual}, {mode:?}");
                let table = unique("runs");
                let body = async {
                    run(
                        &engine.b,
                        &format!("create table {table} (id int primary key)"),
                        ScriptOptions::default(),
                        &[],
                    )
                    .await;
                    engine.a.set_auto_commit(!manual).await.unwrap();
                    let (on_error, answers): (_, &[ErrorAnswer]) = match mode {
                        Mode::Stop => (OnError::Stop, &[]),
                        Mode::Continue => (OnError::Continue, &[]),
                        Mode::AskSkip => (OnError::Ask, &[ErrorAnswer::Skip]),
                        Mode::AskStop => (OnError::Ask, &[ErrorAnswer::Stop]),
                    };
                    let out = run(
                        &engine.a,
                        &three_of_five(&table),
                        options(on_error),
                        answers,
                    )
                    .await;
                    let summary = out.summary;
                    assert_eq!(summary.statements, 5, "{case}");
                    assert_eq!(out.failed, [2], "{case}");
                    assert!(
                        out.timing_ok,
                        "{case}: execution and fetch fit in the statement"
                    );

                    // Whether the run went past the failed statement.
                    let aborted = manual && engine.aborts_transactions();
                    let went_on = !aborted && matches!(mode, Mode::Continue | Mode::AskSkip);
                    let asked = !aborted && matches!(mode, Mode::AskSkip | Mode::AskStop);
                    assert_eq!(
                        out.paused,
                        if asked { vec![(2, 2)] } else { vec![] },
                        "{case}"
                    );
                    if went_on {
                        assert_eq!(out.finished, [0, 1, 3, 4], "{case}");
                        assert_eq!((summary.executed, summary.succeeded), (5, 4), "{case}");
                        assert_eq!(summary.stop, None, "{case}");
                    } else {
                        assert_eq!(out.finished, [0, 1], "{case}");
                        assert_eq!((summary.executed, summary.succeeded), (3, 2), "{case}");
                        assert_eq!(summary.not_run(), 2, "{case}");
                        let expected = if aborted && mode != Mode::Stop {
                            ScriptStop::TransactionAborted { index: 2 }
                        } else {
                            ScriptStop::Failed { index: 2 }
                        };
                        assert_eq!(summary.stop, Some(expected), "{case}");
                    }

                    if manual {
                        // Nothing is committed until the user commits.
                        assert_eq!(count(&engine, &table).await, "0", "{case}");
                        let status = engine.a.transaction_status();
                        if aborted {
                            assert_eq!(status.state, Some(TransactionState::Failed), "{case}");
                            engine.a.rollback().await.unwrap();
                            assert_eq!(count(&engine, &table).await, "0", "{case}");
                        } else {
                            assert_eq!(status.state, Some(TransactionState::Open), "{case}");
                            engine.a.commit().await.unwrap();
                            let kept = if went_on { "4" } else { "2" };
                            assert_eq!(count(&engine, &table).await, kept, "{case}");
                        }
                    } else {
                        assert_eq!(
                            engine.a.transaction_status().state,
                            Some(TransactionState::Idle),
                            "{case}"
                        );
                        let kept = if went_on { "4" } else { "2" };
                        assert_eq!(count(&engine, &table).await, kept, "{case}");
                    }
                };
                cleaning_up(body, |panicked| {
                    drop_tables(&engine, vec![table.clone()], panicked)
                })
                .await;
            }
        }
    }
}

#[tokio::test]
async fn skip_all_does_not_ask_again() {
    for engine in engines().await {
        let name = engine.name;
        let table = unique("runs_all");
        let body = async {
            run(
                &engine.a,
                &format!("create table {table} (id int primary key)"),
                ScriptOptions::default(),
                &[],
            )
            .await;
            let script = [1, 1, 1, 2, 3]
                .map(|id| format!("insert into {table} values ({id})"))
                .join(";\n");
            let out = run(
                &engine.a,
                &script,
                options(OnError::Ask),
                &[ErrorAnswer::SkipAll],
            )
            .await;
            assert_eq!(out.paused, [(1, 3)], "{name}");
            assert_eq!(out.failed, [1, 2], "{name}");
            assert_eq!(out.finished, [0, 3, 4], "{name}");
            assert_eq!(out.summary.stop, None, "{name}");
            assert_eq!(count(&engine, &table).await, "3", "{name}");
        };
        cleaning_up(body, |panicked| {
            drop_tables(&engine, vec![table.clone()], panicked)
        })
        .await;
    }
}

/// Runs until the first pause and returns the stream (still paused) and its ticket.
async fn until_paused(
    session: &Session,
    sql: &str,
) -> (
    futures_util::stream::BoxStream<'static, CoreEvent>,
    ErrorTicket,
) {
    let mut events = session.run_script_with(sql, options(OnError::Ask));
    while let Some(event) = events.next().await {
        if let CoreEvent::ErrorPaused { ticket, .. } = event {
            return (events, ticket);
        }
    }
    panic!("the run never paused");
}

async fn summary_of(
    mut events: futures_util::stream::BoxStream<'static, CoreEvent>,
) -> ScriptSummary {
    let mut finished = None;
    while let Some(event) = events.next().await {
        if let CoreEvent::ScriptFinished { summary, .. } = event {
            finished = Some(summary);
        }
    }
    finished.expect("the run finished")
}

#[tokio::test]
async fn a_paused_run_takes_only_its_ticket_and_stops_when_cancelled_or_closed() {
    for engine in engines().await {
        let name = engine.name;
        let table = unique("runs_pause");
        let body = async {
            run(
                &engine.a,
                &format!("create table {table} (id int primary key)"),
                ScriptOptions::default(),
                &[],
            )
            .await;
            let script = three_of_five(&table);

            // Only the current ticket answers, once.
            let (events, ticket) = until_paused(&engine.a, &script).await;
            assert_eq!(
                engine
                    .a
                    .answer_error(ErrorTicket(ticket.0 + 1), ErrorAnswer::Skip),
                Err(AnswerError::NotWaiting),
                "{name}"
            );
            engine.a.answer_error(ticket, ErrorAnswer::Stop).unwrap();
            assert_eq!(
                engine.a.answer_error(ticket, ErrorAnswer::Skip),
                Err(AnswerError::NotWaiting),
                "{name}"
            );
            let summary = summary_of(events).await;
            assert_eq!(
                summary.stop,
                Some(ScriptStop::Failed { index: 2 }),
                "{name}"
            );
            run(
                &engine.a,
                &format!("delete from {table}"),
                ScriptOptions::default(),
                &[],
            )
            .await;

            // Cancel stops it without running anything else; the old ticket is dead.
            let (events, ticket) = until_paused(&engine.a, &script).await;
            engine.a.cancel().await.unwrap();
            let summary = summary_of(events).await;
            assert_eq!(
                summary.stop,
                Some(ScriptStop::Cancelled { index: 2 }),
                "{name}"
            );
            assert_eq!(summary.not_run(), 2, "{name}");
            assert_eq!(
                engine.a.answer_error(ticket, ErrorAnswer::Skip),
                Err(AnswerError::NotWaiting),
                "{name}"
            );
            assert_eq!(count(&engine, &table).await, "2", "{name}");
            run(
                &engine.a,
                &format!("delete from {table}"),
                ScriptOptions::default(),
                &[],
            )
            .await;

            // Closing the tab stops it too.
            let (events, _) = until_paused(&engine.a, &script).await;
            assert!(engine.a.stop_paused_run(), "{name}");
            let summary = summary_of(events).await;
            assert_eq!(
                summary.stop,
                Some(ScriptStop::Closed { index: 2 }),
                "{name}"
            );
            assert!(
                !engine.a.stop_paused_run(),
                "{name}: nothing waits any more"
            );

            // The session still runs scripts afterwards.
            let out = run(
                &engine.a,
                &format!("select count(*) from {table}"),
                ScriptOptions::default(),
                &[],
            )
            .await;
            assert_eq!(out.finished, [0], "{name}");
        };
        cleaning_up(body, |panicked| {
            drop_tables(&engine, vec![table.clone()], panicked)
        })
        .await;
    }
}

#[tokio::test]
async fn a_cancelled_run_stops_in_every_mode() {
    for engine in engines().await {
        let name = engine.name;
        let slow = slow_statement(name);
        for on_error in [OnError::Continue, OnError::Ask] {
            let script = format!("{slow};\nselect 1");
            let consume = async {
                let mut events = engine.a.run_script_with(&script, options(on_error));
                let mut finished = Vec::new();
                let mut summary = None;
                while let Some(event) = events.next().await {
                    match event {
                        CoreEvent::StatementFinished { index, .. } => finished.push(index),
                        CoreEvent::ErrorPaused { .. } => panic!("{name}: a cancel does not ask"),
                        CoreEvent::ScriptFinished { summary: s, .. } => summary = Some(s),
                        _ => {}
                    }
                }
                (finished, summary)
            };
            let cancel = async {
                // Give the statement time to reach the server, then cancel.
                tokio::time::sleep(Duration::from_millis(800)).await;
                engine.a.cancel().await.unwrap();
            };
            // A lost cancel fails here instead of waiting for the slow statement to end.
            let ((finished, summary), ()) = tokio::time::timeout(Duration::from_secs(60), async {
                tokio::join!(consume, cancel)
            })
            .await
            .unwrap_or_else(|_| panic!("{name} {on_error:?}: the cancel was lost"));
            let summary = summary.unwrap();
            assert!(
                finished.is_empty(),
                "{name} {on_error:?}: nothing after the cancel"
            );
            assert_eq!(
                summary.stop,
                Some(ScriptStop::Cancelled { index: 0 }),
                "{name} {on_error:?}"
            );
        }
    }
}

#[tokio::test]
async fn continue_after_a_failed_write_under_smart_commit() {
    const SMART: Safeguards = Safeguards {
        confirm_execution: false,
        confirm_data_changes: true,
        close_idle_after: None,
        manual_commit: false,
        smart_commit: true,
    };
    for engine in engines().await {
        let name = engine.name;
        let table = unique("runs_smart");
        let body = async {
            run(
                &engine.b,
                &format!("create table {table} (id int primary key)"),
                ScriptOptions::default(),
                &[],
            )
            .await;
            run(
                &engine.b,
                &format!("insert into {table} values (1)"),
                ScriptOptions::default(),
                &[],
            )
            .await;
            let status = engine.a.follow_transactions(&SMART).await;
            assert!(status.auto_commit, "{name}");
            // The first write switches to manual and fails; the rest goes on in its transaction.
            let script = format!(
                "select count(*) from {table};\ninsert into {table} values (1);\ninsert into {table} values (2)"
            );
            let out = run(&engine.a, &script, options(OnError::Continue), &[]).await;
            assert_eq!(out.failed, [1], "{name}");
            let status = engine.a.transaction_status();
            if engine.aborts_transactions() {
                assert_eq!(
                    out.summary.stop,
                    Some(ScriptStop::TransactionAborted { index: 1 }),
                    "{name}"
                );
                assert_eq!(status.state, Some(TransactionState::Failed), "{name}");
                engine.a.rollback().await.unwrap();
            } else {
                assert_eq!(out.finished, [0, 2], "{name}");
                assert!(status.smart_switched && !status.auto_commit, "{name}");
                assert_eq!(status.state, Some(TransactionState::Open), "{name}");
                // SQL Server's READPAST would skip the row the failed insert locked.
                if name != "sqlserver" {
                    assert_eq!(
                        count(&engine, &table).await,
                        "1",
                        "{name}: waits for Commit"
                    );
                }
                engine.a.commit().await.unwrap();
                assert_eq!(count(&engine, &table).await, "2", "{name}");
            }
            let status = engine.a.transaction_status();
            assert!(
                status.auto_commit && !status.smart_switched,
                "{name}: back to auto-commit"
            );
        };
        cleaning_up(body, |panicked| {
            drop_tables(&engine, vec![table.clone()], panicked)
        })
        .await;
    }
}

#[tokio::test]
async fn an_error_that_rolls_back_the_transaction_stops_continue() {
    // SQL Server with XACT_ABORT rolls the whole transaction back on any error.
    for engine in engines().await {
        if engine.name != "sqlserver" {
            continue;
        }
        let table = unique("runs_xact");
        let body = async {
            run(
                &engine.b,
                &format!("create table {table} (id int primary key)"),
                ScriptOptions::default(),
                &[],
            )
            .await;
            engine.a.follow_transactions(&Safeguards::default()).await;
            run(
                &engine.a,
                "set xact_abort on",
                ScriptOptions::default(),
                &[],
            )
            .await;
            engine.a.set_auto_commit(false).await.unwrap();
            for on_error in [OnError::Continue, OnError::Ask] {
                let out = run(&engine.a, &three_of_five(&table), options(on_error), &[]).await;
                assert_eq!(out.failed, [2]);
                assert!(out.paused.is_empty(), "{on_error:?}: nothing to ask");
                assert_eq!(
                    out.summary.stop,
                    Some(ScriptStop::TransactionRolledBack { index: 2 })
                );
                assert!(
                    out.notices
                        .iter()
                        .any(|n| n.contains("rolled back by this error")),
                    "{:?}",
                    out.notices
                );
                assert_eq!(
                    engine.a.transaction_status().state,
                    Some(TransactionState::Idle)
                );
                assert_eq!(count(&engine, &table).await, "0");
            }
            engine.a.set_auto_commit(true).await.unwrap();
            run(
                &engine.a,
                "set xact_abort off",
                ScriptOptions::default(),
                &[],
            )
            .await;
        };
        cleaning_up(body, |panicked| {
            drop_tables(&engine, vec![table.clone()], panicked)
        })
        .await;
    }
}

/// A statement that runs far longer than the tests wait, on each engine.
fn slow_statement(engine: &str) -> &'static str {
    match engine {
        "postgres" => "select pg_sleep(60)",
        // `SLEEP` returns 1 when killed instead of failing: a long read fails as interrupted.
        "mysql" | "mariadb" => {
            "select count(*) from information_schema.columns a, information_schema.columns b, information_schema.columns c"
        }
        "sqlserver" => "waitfor delay '00:01:00'",
        "sqlite" => {
            "with recursive r(i) as (select 1 union all select i + 1 from r) select count(*) from r"
        }
        "duckdb" => "select sum(a.range * b.range) from range(1000000) a, range(1000000) b",
        other => panic!("no slow statement for {other}"),
    }
}

#[tokio::test]
async fn a_statement_that_runs_too_long_times_out_on_every_engine() {
    for engine in engines().await {
        let name = engine.name;
        let script = format!("{};\nselect 1", slow_statement(name));
        let started = Instant::now();
        // The timeout's cancel must stop the statement: a lost one fails here.
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            run(
                &engine.a,
                &script,
                ScriptOptions {
                    on_error: OnError::Continue,
                    statement_timeout: Some(Duration::from_millis(700)),
                },
                &[],
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{name}: the statement timeout's cancel was lost"));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{name}: took {:?}",
            started.elapsed()
        );
        assert_eq!(out.timed_out, [0], "{name}: {out:?}");
        assert_eq!(
            out.finished,
            [1],
            "{name}: the session goes on after the timeout"
        );
        assert_eq!(out.summary.stop, None, "{name}");

        // In stop mode the timeout stops the run like any error.
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            run(
                &engine.a,
                &script,
                ScriptOptions {
                    on_error: OnError::Stop,
                    statement_timeout: Some(Duration::from_millis(700)),
                },
                &[],
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{name}: the statement timeout's cancel was lost"));
        assert_eq!(out.timed_out, [0], "{name}");
        assert_eq!(
            out.summary.stop,
            Some(ScriptStop::Failed { index: 0 }),
            "{name}"
        );

        // A quick statement is not touched by its timeout.
        let out = run(
            &engine.a,
            "select 1",
            ScriptOptions {
                on_error: OnError::Stop,
                statement_timeout: Some(Duration::from_secs(30)),
            },
            &[],
        )
        .await;
        assert_eq!(out.finished, [0], "{name}");
    }
}
