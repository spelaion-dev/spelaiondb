//! A plugin's process has one state (SDB-373, ADR-060): not started, starting, running,
//! stopping or dead with why. These tests start the made-up engine's program, as
//! `tests/protocol.rs` does.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_driver_api::{ConnectParams, Connection, Driver, ExecOptions, Secret};
use spelaion_plugin_host::{Plugin, ProcessState};

const PROGRAM: &str = env!("CARGO_BIN_EXE_spelaion-test-plugin");

async fn within<T>(what: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), what)
        .await
        .expect("the plugin did not answer in time")
}

fn params(database: &str) -> ConnectParams {
    ConnectParams {
        host: "db.test".into(),
        port: 5555,
        user: "app".into(),
        password: Some(Secret::new("not-a-real-password")),
        database: database.into(),
        ..Default::default()
    }
}

async fn connect(plugin: &Plugin, database: &str) -> Box<dyn Connection> {
    within(plugin.driver("test").connect(&params(database)))
        .await
        .expect("connecting")
}

/// Runs `sql` to its end, whatever it returns.
async fn run(connection: &dyn Connection, sql: &str) {
    let events = connection
        .execute(sql, &ExecOptions::default())
        .await
        .expect("the statement starts");
    let _: Vec<_> = within(events.collect()).await;
}

/// Waits until the plugin is in a state `wanted` accepts; the last state seen if it never is.
async fn until(plugin: &Plugin, wanted: impl Fn(&ProcessState) -> bool) -> ProcessState {
    for _ in 0..400 {
        let state = plugin.state();
        if wanted(&state) {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    plugin.state()
}

fn dead(state: &ProcessState) -> Option<&str> {
    match state {
        ProcessState::Dead { why } => Some(why),
        ProcessState::NotStarted
        | ProcessState::Starting
        | ProcessState::Running
        | ProcessState::Stopping => None,
    }
}

#[tokio::test]
async fn a_process_that_dies_is_dead_at_once_with_why_and_the_next_connection_starts_another() {
    let plugin = Plugin::new(PROGRAM);
    assert_eq!(plugin.state(), ProcessState::NotStarted);
    let doomed = connect(&plugin, "made").await;
    assert_eq!(plugin.state(), ProcessState::Running);

    run(doomed.as_ref(), "crash").await;
    // Nobody connects again: the state says it at once, not at the next call.
    let state = until(&plugin, |state| dead(state).is_some()).await;
    let why = dead(&state).expect("dead");
    assert!(!why.is_empty(), "dead without a reason");
    assert!(!within(plugin.is_running()).await);
    assert!(within(plugin.usage()).await.is_none());

    // Nothing restarts it on its own; the next connection does, and it runs.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(dead(&plugin.state()).is_some());
    let _again = connect(&plugin, "made").await;
    assert_eq!(plugin.state(), ProcessState::Running);
    within(plugin.stop()).await;
}

#[tokio::test]
async fn stopping_is_a_state_of_its_own_and_ends_dead_because_it_was_stopped() {
    // A driver that never finishes closing: stopping lasts the plugin's grace.
    let path = std::env::temp_dir().join(format!("spelaion-state-hangs-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin, &format!("file-hang:{}", path.display())).await;
    run(connection.as_ref(), "keep never-written").await;
    drop(connection);

    let stopping = tokio::spawn({
        let plugin = plugin.clone();
        async move { within(plugin.stop()).await }
    });
    assert_eq!(
        until(&plugin, |state| *state == ProcessState::Stopping).await,
        ProcessState::Stopping
    );
    assert!(!within(plugin.is_running()).await);
    stopping.await.unwrap();
    assert_eq!(
        plugin.state(),
        ProcessState::Dead {
            why: "the plugin was stopped".into()
        }
    );
    let _ = std::fs::remove_file(&path);

    // Stopping a plugin that does not run changes nothing.
    let idle = Plugin::new(PROGRAM);
    within(idle.stop()).await;
    assert_eq!(idle.state(), ProcessState::NotStarted);
}

#[tokio::test]
async fn a_refused_file_is_dead_with_why_and_is_checked_again_at_the_next_start() {
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let plugin = Plugin::new(PROGRAM).with_check({
        let allow = allow.clone();
        let checks = checks.clone();
        Arc::new(move |_: &Path| {
            checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if allow.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(())
            } else {
                Err("its SHA-256 is not the pinned one".into())
            }
        })
    });
    let refused = within(plugin.driver("test").connect(&params("made"))).await;
    assert!(refused.is_err());
    let state = plugin.state();
    let why = dead(&state).expect("a refused start is dead");
    assert!(why.contains("was not started"), "{why}");
    assert!(why.contains("not the pinned one"), "{why}");

    // A refusal is not remembered: the file is checked again, every start.
    allow.store(true, std::sync::atomic::Ordering::SeqCst);
    let _connection = connect(&plugin, "made").await;
    assert_eq!(plugin.state(), ProcessState::Running);
    assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 2);
    within(plugin.stop()).await;
    let _again = connect(&plugin, "made").await;
    assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 3);
    within(plugin.stop()).await;
}

#[tokio::test]
async fn a_start_is_starting_until_it_answers_and_a_start_given_up_leaves_it_not_started() {
    let release = Arc::new(std::sync::Barrier::new(2));
    let plugin = Plugin::new(PROGRAM).with_check({
        let release = release.clone();
        Arc::new(move |_: &Path| {
            release.wait();
            Ok(())
        })
    });
    let connecting = tokio::spawn({
        let plugin = plugin.clone();
        async move { within(plugin.driver("test").connect(&params("made"))).await }
    });
    assert_eq!(
        until(&plugin, |state| *state == ProcessState::Starting).await,
        ProcessState::Starting
    );
    assert!(!within(plugin.is_running()).await);
    release.wait();
    let _connection = connecting.await.unwrap().expect("connecting");
    assert_eq!(plugin.state(), ProcessState::Running);
    within(plugin.stop()).await;

    // A connection that stops waiting while the file is checked does not leave it "starting".
    let hold = Arc::new(std::sync::Barrier::new(2));
    let slow = Plugin::new(PROGRAM).with_check({
        let hold = hold.clone();
        Arc::new(move |_: &Path| {
            hold.wait();
            Ok(())
        })
    });
    let given_up = tokio::spawn({
        let slow = slow.clone();
        async move { slow.driver("test").connect(&params("made")).await }
    });
    assert_eq!(
        until(&slow, |state| *state == ProcessState::Starting).await,
        ProcessState::Starting
    );
    given_up.abort();
    let _ = given_up.await;
    assert_eq!(slow.state(), ProcessState::NotStarted);
    // Let the check it was waiting on end.
    hold.wait();
}
