//! What every plugin must do, checked against a plugin's own program and a server of its
//! engine (ADR-042): the set a plugin runs against itself in its own CI.
//!
//! It checks the protocol, not the engine: that the program starts, speaks this version, opens
//! sessions, returns results in the order the contract says, fails without hanging, obeys a
//! cancel while a statement runs and ends when its input closes. What an engine's types look
//! like is its own tests' business.
//!
//! A plugin of an engine queried with SQL is checked with statements ([`Target`]); one whose
//! language is not SQL (ADR-056) with requests ([`RequestTarget`]): the same checks.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use spelaion_driver_api::{
    ConnectParams, Connection, Driver, DriverError, EventStream, ExecEvent, ExecOptions,
};
use spelaion_plugin_protocol::REQUESTS;

use crate::{Plugin, VERSION, Version};

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

/// A plugin of an engine whose language is not SQL (ADR-056), checked through requests
/// (protocol 1.1): the same checks as [`Target`]'s, each statement a canonical request.
pub struct RequestTarget {
    /// The plugin's program.
    pub program: PathBuf,
    /// One of its engines, by the name the configuration uses.
    pub engine: String,
    pub params: ConnectParams,
    /// A request that returns exactly one row.
    pub one_row: Value,
    /// A request the server refuses.
    pub refused: Value,
    /// A request that runs for at least ten seconds unless it is cancelled, for engines that
    /// can cancel. `None` skips that check.
    pub slow: Option<Value>,
}

/// What one check sends: a statement's text, or a request.
enum Probe {
    Sql(String),
    Request(Value),
}

impl std::fmt::Display for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Probe::Sql(sql) => write!(f, "{sql:?}"),
            Probe::Request(request) => write!(f, "{request}"),
        }
    }
}

impl Probe {
    async fn start(&self, connection: &dyn Connection) -> Result<EventStream, DriverError> {
        let options = ExecOptions::default();
        match self {
            Probe::Sql(sql) => connection.execute(sql, &options).await,
            Probe::Request(request) => connection.execute_request(request.clone(), &options).await,
        }
    }
}

/// What both kinds of target are checked with.
struct Checked<'a> {
    program: &'a Path,
    engine: &'a str,
    params: &'a ConnectParams,
    one_row: Probe,
    refused: Probe,
    slow: Option<Probe>,
    /// The protocol version the plugin must have chosen at least.
    at_least: Version,
}

/// How long any one step may take.
const STEP: Duration = Duration::from_secs(30);

/// Everything `target` does that the protocol does not allow. Empty: it conforms.
pub async fn check(target: &Target) -> Vec<String> {
    check_with(Checked {
        program: &target.program,
        engine: &target.engine,
        params: &target.params,
        one_row: Probe::Sql(target.one_row.clone()),
        refused: Probe::Sql(target.refused.clone()),
        slow: target.slow.clone().map(Probe::Sql),
        at_least: Version {
            major: VERSION.major,
            minor: 0,
        },
    })
    .await
}

/// Everything a plugin of requests does that the protocol does not allow. Empty: it conforms.
pub async fn check_requests(target: &RequestTarget) -> Vec<String> {
    check_with(Checked {
        program: &target.program,
        engine: &target.engine,
        params: &target.params,
        one_row: Probe::Request(target.one_row.clone()),
        refused: Probe::Request(target.refused.clone()),
        slow: target.slow.clone().map(Probe::Request),
        at_least: REQUESTS,
    })
    .await
}

async fn check_with(target: Checked<'_>) -> Vec<String> {
    let mut failures = Vec::new();
    let plugin = Plugin::new(target.program);

    match within(plugin.describe()).await {
        Some(Ok(info)) => {
            if info.protocol.major != VERSION.major {
                failures.push(format!(
                    "it speaks version {} of the protocol, not {VERSION}",
                    info.protocol
                ));
            } else if info.protocol < target.at_least {
                failures.push(format!(
                    "it speaks version {} of the protocol; requests need {}",
                    info.protocol, target.at_least
                ));
            }
            if !info.engines.iter().any(|e| e == target.engine) {
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

    let driver = plugin.driver(target.engine);
    let connection = match within(driver.connect(target.params)).await {
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

    one_row(connection, &target.one_row, &mut failures).await;

    // A refused statement fails, before it starts or in its stream, and never hangs.
    match within(run(connection, &target.refused)).await {
        Some(events) if events.iter().any(Result::is_err) => {}
        Some(_) => failures.push(format!("{} was not refused", target.refused)),
        None => failures.push(format!("{} was never answered", target.refused)),
    }
    // And the session goes on.
    one_row(connection, &target.one_row, &mut failures).await;

    match within(connection.server_version()).await {
        Some(Ok(version)) if !version.trim().is_empty() => {}
        Some(Ok(_)) => failures.push("the server's version is empty".into()),
        Some(Err(error)) => failures.push(format!("`server_version` failed: {error}")),
        None => failures.push("`server_version` was never answered".into()),
    }
    match within(connection.list_schemas()).await {
        Some(Ok(schemas)) => {
            if let Some(schema) = schemas.iter().find(|s| !s.system) {
                batched_tables(connection, &schema.name, &mut failures).await;
            }
        }
        Some(Err(error)) => failures.push(format!("`list_schemas` failed: {error}")),
        None => failures.push("`list_schemas` was never answered".into()),
    }

    // A second session of the same process works while the first is there.
    match within(driver.connect(target.params)).await {
        Some(Ok(second)) => one_row(second.as_ref(), &target.one_row, &mut failures).await,
        Some(Err(error)) => failures.push(format!("a second session did not connect: {error}")),
        None => failures.push("a second `connect` was never answered".into()),
    }

    if let Some(slow) = &target.slow
        && connection.capabilities().cancel
    {
        cancel(connection, slow, &mut failures).await;
        one_row(connection, &target.one_row, &mut failures).await;
    }

    drop(plugin);
    if let Err(failure) = ends_with_its_input(target.program).await {
        failures.push(failure);
    }
    failures
}

/// Panics with everything `target` does wrong: for a test.
pub async fn assert_conforms(target: &Target) {
    assert_none(check(target).await);
}

/// Panics with everything a plugin of requests does wrong: for a test.
pub async fn assert_conforms_requests(target: &RequestTarget) {
    assert_none(check_requests(target).await);
}

fn assert_none(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "the plugin does not conform to the protocol:\n- {}",
        failures.join("\n- ")
    );
}

async fn within<T>(step: impl Future<Output = T>) -> Option<T> {
    tokio::time::timeout(STEP, step).await.ok()
}

async fn run(connection: &dyn Connection, probe: &Probe) -> Vec<Result<ExecEvent, DriverError>> {
    match probe.start(connection).await {
        Ok(events) => events.collect().await,
        Err(error) => vec![Err(error)],
    }
}

/// Most tables [`batched_tables`] compares one by one: enough to catch a batched read that
/// differs, without describing a whole large schema twice in a plugin's CI.
const MAX_COMPARED_TABLES: usize = 100;

/// `describe_tables` (1.4, SDB-220), where the plugin answers it, says of each table exactly
/// what `describe_table` says, and only of tables `list_objects` lists.
async fn batched_tables(connection: &dyn Connection, schema: &str, failures: &mut Vec<String>) {
    let tables = match within(connection.describe_tables(schema)).await {
        Some(Ok(Some(tables))) => tables,
        // No batched read: each table is described on its own, which the core does then.
        Some(Ok(None)) => return,
        Some(Err(error)) => {
            failures.push(format!("`describe_tables` of {schema:?} failed: {error}"));
            return;
        }
        None => {
            failures.push(format!(
                "`describe_tables` of {schema:?} was never answered"
            ));
            return;
        }
    };
    let listed = match within(connection.list_objects(schema)).await {
        Some(Ok(objects)) => objects,
        _ => return,
    };
    for table in tables.iter().take(MAX_COMPARED_TABLES) {
        let name = &table.object.name;
        if !listed.iter().any(|o| &o.name == name) {
            failures.push(format!(
                "`describe_tables` of {schema:?} has {name:?}, which `list_objects` does not list"
            ));
            continue;
        }
        match within(connection.describe_table(schema, name)).await {
            Some(Ok(one)) if one == *table => {}
            Some(Ok(_)) => failures.push(format!(
                "`describe_tables` of {schema:?} describes {name:?} unlike `describe_table`"
            )),
            Some(Err(error)) => failures.push(format!(
                "`describe_table` of {name:?}, which `describe_tables` has, failed: {error}"
            )),
            None => failures.push(format!("`describe_table` of {name:?} was never answered")),
        }
    }
}

/// `Columns`, the row, `Done`: in that order, with every column's types.
async fn one_row(connection: &dyn Connection, probe: &Probe, failures: &mut Vec<String>) {
    let Some(events) = within(run(connection, probe)).await else {
        failures.push(format!("{probe} never ended"));
        return;
    };
    let mut columns = None;
    let mut rows = 0;
    let mut done = false;
    for event in &events {
        match event {
            Ok(ExecEvent::Columns(schema)) => {
                if columns.is_some() || rows > 0 || done {
                    failures.push(format!("{probe}: columns arrived twice or late"));
                }
                if schema.columns.iter().any(|c| c.native_type.is_empty()) {
                    failures.push(format!("{probe}: a column has no native type"));
                }
                columns = Some(schema.clone());
            }
            Ok(ExecEvent::Batch(batch)) => {
                match &columns {
                    Some(schema) if batch.schema() == schema.arrow => {}
                    Some(_) => failures.push(format!("{probe}: rows of another schema")),
                    None => failures.push(format!("{probe}: rows before their columns")),
                }
                if done {
                    failures.push(format!("{probe}: rows after the end"));
                }
                rows += batch.num_rows();
            }
            Ok(ExecEvent::Notice(_)) => {}
            Ok(ExecEvent::Done { .. }) => {
                if done {
                    failures.push(format!("{probe}: it ended twice"));
                }
                done = true;
            }
            Err(error) => failures.push(format!("{probe} failed: {error}")),
        }
    }
    if rows != 1 {
        failures.push(format!("{probe} returned {rows} rows, not one"));
    }
    if !done {
        failures.push(format!("{probe} never said it was done"));
    }
}

/// A cancel that arrives while the statement runs ends it, with `Cancelled`.
///
/// The statement and the cancel run side by side: an engine may answer `execute` only when
/// its first response arrives (MySQL's text protocol), which for a statement that waits is
/// when it ends. Waiting for `execute` before cancelling would then wait for ever.
async fn cancel(connection: &dyn Connection, slow: &Probe, failures: &mut Vec<String>) {
    let cancel = connection.cancel_handle();
    let running = async {
        // `Cancelled` may be how it fails to start or an event of its stream.
        let cancelled = |error: &DriverError| matches!(error, DriverError::Cancelled);
        match slow.start(connection).await {
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
        Ok(Ok(false)) => failures.push(format!("{slow} ended without saying it was cancelled")),
        Ok(Err(error)) => failures.push(format!("{slow} did not start: {error}")),
        Err(_) => failures.push(format!("{slow} went on running after `cancel`")),
    }
}

/// The program ends when its standard input closes, by itself.
async fn ends_with_its_input(program: &Path) -> Result<(), String> {
    let mut child = tokio::process::Command::new(program)
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
