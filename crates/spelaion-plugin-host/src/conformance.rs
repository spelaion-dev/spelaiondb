//! What every plugin must do, checked against a plugin's own program and a server of its
//! engine (ADR-042): the set a plugin runs against itself in its own CI.
//!
//! It checks the protocol, not the engine: that the program starts, speaks this version, opens
//! sessions, returns results in the order the contract says, fails without hanging, obeys a
//! cancel while a statement runs and ends when its input closes. What an engine's types look
//! like is its own tests' business.

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_driver_api::{ConnectParams, Connection, Driver, DriverError, ExecEvent, ExecOptions};

use crate::{Plugin, VERSION};

/// A plugin and how to reach a server of its engine.
pub struct Target {
    /// The plugin's program.
    pub program: PathBuf,
    /// One of its engines, by the name the configuration uses.
    pub engine: String,
    pub params: ConnectParams,
    /// A statement that returns exactly one row (`select 1`).
    pub one_row: String,
    /// A statement the server refuses (`select * from a_table_that_is_not_there`).
    pub refused: String,
    /// A statement that runs for at least ten seconds unless it is cancelled (`select
    /// pg_sleep(600)`), for engines that can cancel. `None` skips that check.
    pub slow: Option<String>,
}

/// How long any one step may take.
const STEP: Duration = Duration::from_secs(30);

/// Everything `target` does that the protocol does not allow. Empty: it conforms.
pub async fn check(target: &Target) -> Vec<String> {
    let mut failures = Vec::new();
    let plugin = Plugin::new(&target.program);

    match within(plugin.describe()).await {
        Some(Ok(info)) => {
            if info.protocol.major != VERSION.major {
                failures.push(format!(
                    "it speaks version {} of the protocol, not {VERSION}",
                    info.protocol
                ));
            }
            if !info.engines.contains(&target.engine) {
                failures.push(format!(
                    "it serves {:?}, not {:?}",
                    info.engines, target.engine
                ));
            }
            if info.credit == 0 {
                failures.push("its credit is 0: it could never send a batch".into());
            }
        }
        Some(Err(error)) => {
            failures.push(format!("it did not start: {error}"));
            return failures;
        }
        None => {
            failures.push("it did not answer `initialize`".into());
            return failures;
        }
    }

    let driver = plugin.driver(&target.engine);
    let connection = match within(driver.connect(&target.params)).await {
        Some(Ok(connection)) => connection,
        Some(Err(error)) => {
            failures.push(format!("it did not connect: {error}"));
            return failures;
        }
        None => {
            failures.push("`connect` was never answered".into());
            return failures;
        }
    };
    let connection = connection.as_ref();

    one_row(connection, target, &mut failures).await;

    // A refused statement fails, before it starts or in its stream, and never hangs.
    match within(run(connection, &target.refused)).await {
        Some(events) if events.iter().any(Result::is_err) => {}
        Some(_) => failures.push(format!("{:?} was not refused", target.refused)),
        None => failures.push(format!("{:?} was never answered", target.refused)),
    }
    // And the session goes on.
    one_row(connection, target, &mut failures).await;

    match within(connection.server_version()).await {
        Some(Ok(version)) if !version.trim().is_empty() => {}
        Some(Ok(_)) => failures.push("the server's version is empty".into()),
        Some(Err(error)) => failures.push(format!("`server_version` failed: {error}")),
        None => failures.push("`server_version` was never answered".into()),
    }
    match within(connection.list_schemas()).await {
        Some(Ok(_)) => {}
        Some(Err(error)) => failures.push(format!("`list_schemas` failed: {error}")),
        None => failures.push("`list_schemas` was never answered".into()),
    }

    // A second session of the same process works while the first is there.
    match within(driver.connect(&target.params)).await {
        Some(Ok(second)) => one_row(second.as_ref(), target, &mut failures).await,
        Some(Err(error)) => failures.push(format!("a second session did not connect: {error}")),
        None => failures.push("a second `connect` was never answered".into()),
    }

    if let Some(slow) = &target.slow
        && connection.capabilities().cancel
    {
        cancel(connection, slow, &mut failures).await;
        one_row(connection, target, &mut failures).await;
    }

    drop(plugin);
    if let Err(failure) = ends_with_its_input(target).await {
        failures.push(failure);
    }
    failures
}

/// Panics with everything `target` does wrong: for a test.
pub async fn assert_conforms(target: &Target) {
    let failures = check(target).await;
    assert!(
        failures.is_empty(),
        "the plugin does not conform to the protocol:\n- {}",
        failures.join("\n- ")
    );
}

async fn within<T>(step: impl Future<Output = T>) -> Option<T> {
    tokio::time::timeout(STEP, step).await.ok()
}

async fn run(connection: &dyn Connection, sql: &str) -> Vec<Result<ExecEvent, DriverError>> {
    match connection.execute(sql, &ExecOptions::default()).await {
        Ok(events) => events.collect().await,
        Err(error) => vec![Err(error)],
    }
}

/// `Columns`, the row, `Done`: in that order, with every column's types.
async fn one_row(connection: &dyn Connection, target: &Target, failures: &mut Vec<String>) {
    let sql = &target.one_row;
    let Some(events) = within(run(connection, sql)).await else {
        failures.push(format!("{sql:?} never ended"));
        return;
    };
    let mut columns = None;
    let mut rows = 0;
    let mut done = false;
    for event in &events {
        match event {
            Ok(ExecEvent::Columns(schema)) => {
                if columns.is_some() || rows > 0 || done {
                    failures.push(format!("{sql:?}: columns arrived twice or late"));
                }
                if schema.columns.iter().any(|c| c.native_type.is_empty()) {
                    failures.push(format!("{sql:?}: a column has no native type"));
                }
                columns = Some(schema.clone());
            }
            Ok(ExecEvent::Batch(batch)) => {
                match &columns {
                    Some(schema) if batch.schema() == schema.arrow => {}
                    Some(_) => failures.push(format!("{sql:?}: rows of another schema")),
                    None => failures.push(format!("{sql:?}: rows before their columns")),
                }
                if done {
                    failures.push(format!("{sql:?}: rows after the end"));
                }
                rows += batch.num_rows();
            }
            Ok(ExecEvent::Notice(_)) => {}
            Ok(ExecEvent::Done { .. }) => {
                if done {
                    failures.push(format!("{sql:?}: it ended twice"));
                }
                done = true;
            }
            Err(error) => failures.push(format!("{sql:?} failed: {error}")),
        }
    }
    if rows != 1 {
        failures.push(format!("{sql:?} returned {rows} rows, not one"));
    }
    if !done {
        failures.push(format!("{sql:?} never said it was done"));
    }
}

/// A cancel that arrives while the statement runs ends it, with `Cancelled`.
///
/// The statement and the cancel run side by side: an engine may answer `execute` only when
/// its first response arrives (MySQL's text protocol), which for a statement that waits is
/// when it ends. Waiting for `execute` before cancelling would then wait for ever.
async fn cancel(connection: &dyn Connection, slow: &str, failures: &mut Vec<String>) {
    let cancel = connection.cancel_handle();
    let running = async {
        // `Cancelled` may be how it fails to start or an event of its stream.
        let cancelled = |error: &DriverError| matches!(error, DriverError::Cancelled);
        match connection.execute(slow, &ExecOptions::default()).await {
            Ok(mut events) => {
                let mut said = false;
                while let Some(event) = events.next().await {
                    said |= event.as_ref().is_err_and(cancelled);
                }
                Ok(said)
            }
            Err(error) if cancelled(&error) => Ok(true),
            Err(error) => Err(error),
        }
    };
    let cancelling = async {
        // Let it reach the server.
        tokio::time::sleep(Duration::from_millis(500)).await;
        within(cancel.cancel()).await
    };
    let (ended, answered) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(15), running),
        cancelling
    );
    match answered {
        Some(Ok(())) => {}
        Some(Err(error)) => failures.push(format!("`cancel` failed: {error}")),
        None => failures.push("`cancel` was never answered".into()),
    }
    match ended {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => failures.push(format!("{slow:?} ended without saying it was cancelled")),
        Ok(Err(error)) => failures.push(format!("{slow:?} did not start: {error}")),
        Err(_) => failures.push(format!("{slow:?} went on running after `cancel`")),
    }
}

/// The program ends when its standard input closes, by itself.
async fn ends_with_its_input(target: &Target) -> Result<(), String> {
    let mut child = tokio::process::Command::new(&target.program)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("it could not be started again: {e}"))?;
    drop(child.stdin.take());
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("waiting for it failed: {error}")),
        Err(_) => Err("it outlived its standard input".into()),
    }
}
