//! Cleaning up after the tests that create objects in the shared test databases (the Docker
//! servers every test run and the smoke test use): a failing test must not leave its schemas,
//! tables or roles behind, where every later run, and the smoke test's explorer, would find them.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};
use spelaion_core::{CoreEvent, Session};

/// How long a cleanup step may wait (for a lock, a lost server) before it gives up.
pub const CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs `body`, then `cleanup` whether `body` finished or panicked (a failed assertion), and
/// only then lets the panic go on. `cleanup` is told whether `body` panicked, so it can first
/// undo what the body left half done (an open transaction holding a table) and not fail a
/// second time.
pub async fn cleaning_up<T, C>(body: impl Future<Output = T>, cleanup: impl FnOnce(bool) -> C) -> T
where
    C: Future<Output = ()>,
{
    let outcome = AssertUnwindSafe(body).catch_unwind().await;
    cleanup(outcome.is_err()).await;
    match outcome {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Runs each cleanup statement on `session`. A failure fails the test, unless the test already
/// failed (`panicked`): then it is only said, so the test's own failure stays the one reported.
pub async fn run_cleanup(label: &str, session: &Session, statements: &[String], panicked: bool) {
    for sql in statements {
        let events = tokio::time::timeout(
            CLEANUP_TIMEOUT,
            session.run_script(sql).collect::<Vec<CoreEvent>>(),
        )
        .await;
        let error = match &events {
            Ok(events) => events.iter().find_map(|event| match event {
                CoreEvent::StatementFailed { error, .. } => Some(error.message.clone()),
                _ => None,
            }),
            Err(_) => Some("timed out".into()),
        };
        match error {
            None => {}
            Some(error) if panicked => eprintln!("{label}: cleanup `{sql}` failed: {error}"),
            Some(error) => panic!("{label}: cleanup `{sql}` failed: {error}"),
        }
    }
}

/// A temporary folder for this run of the tests, named after `prefix` and the process id, and
/// emptied the first time this run asks for it (SDB-539). Windows reuses process ids, so a
/// folder an earlier run left with the same name (an export that would now be written beside it
/// as `name (2).xlsx`) is gone before any test of this run writes in it; later calls in the
/// same run keep what earlier ones wrote.
// Used by the core binary's files; the binaries that run apart (`postgres_family`…) share
// this module without it.
#[allow(dead_code)]
pub fn run_dir(prefix: &str) -> std::path::PathBuf {
    static CLEARED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    let mut cleared = CLEARED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cleared.insert(prefix.to_string()) {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
