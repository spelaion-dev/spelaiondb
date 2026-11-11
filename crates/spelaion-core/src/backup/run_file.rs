//! Running an `.sql` file against a connection through our own session (ADR-049): read with the
//! editor's decoding (ADR-036), split by the engine's dialect, refused on a read-only connection
//! and unless confirmed, and run statement by statement in a session of its own (never a tab's),
//! with progress as statements done of the total. Its statements are not recorded in the query
//! history: the caller's session is not the history's, and the file is the record.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;

use super::process::{BackupCancel, BackupEvent};
use super::{BackupError, io_error};
use crate::guard::{Risk, script_risks};
use crate::script::{OnError, ScriptOptions};
use crate::{CoreEvent, Session};

/// The largest file run: it is read and held whole, once.
pub const RUN_FILE_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// What the confirmation shows before a file runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlan {
    pub path: PathBuf,
    pub bytes: u64,
    /// Statements, as the engine's splitter counts them.
    pub statements: usize,
    /// Statements the guard would ask about in the editor (DROP, DELETE without WHERE…), and
    /// those the connection's type asks about, by kind.
    pub risks: Vec<(Risk, usize)>,
}

/// What a finished run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRunSummary {
    pub done: u64,
    pub failed: u64,
    pub total: u64,
    pub elapsed: Duration,
}

fn read(path: &Path) -> Result<(String, u64), BackupError> {
    let meta = std::fs::metadata(path).map_err(io_error("read", path))?;
    if meta.is_dir() {
        return Err(BackupError::Invalid(format!(
            "{} is a folder",
            path.display()
        )));
    }
    if meta.len() > RUN_FILE_MAX_BYTES {
        return Err(BackupError::Invalid(format!(
            "the file is too large to run here ({} bytes; at most {RUN_FILE_MAX_BYTES}): use the database's own client",
            meta.len()
        )));
    }
    let bytes = std::fs::read(path).map_err(io_error("read", path))?;
    let (text, _) =
        crate::sql_file::decode(&bytes).map_err(|e| BackupError::Invalid(e.to_string()))?;
    Ok((text, bytes.len() as u64))
}

/// Reads the file and counts its statements and risks, for the confirmation.
pub fn plan_sql_file(
    session: &Session,
    path: &Path,
    safeguards: &spelaion_config::Safeguards,
) -> Result<FilePlan, BackupError> {
    let (text, bytes) = read(path)?;
    let dialect = session.dialect();
    let statements = dialect.split_statements(&text).len();
    let mut risks: Vec<(Risk, usize)> = Vec::new();
    for risky in script_risks(dialect.as_ref(), &text, safeguards) {
        match risks.iter_mut().find(|(r, _)| *r == risky.risk) {
            Some((_, n)) => *n += 1,
            None => risks.push((risky.risk, 1)),
        }
    }
    Ok(FilePlan {
        path: path.to_path_buf(),
        bytes,
        statements,
        risks,
    })
}

const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// Runs the file in `session` (a session of its own). Refused on a read-only connection and
/// unless `confirmed`. A cancel stops the statement running and the rest.
pub async fn run_sql_file(
    session: &Session,
    path: &Path,
    read_only: bool,
    confirmed: bool,
    on_error: OnError,
    cancel: &BackupCancel,
    events: &(dyn Fn(BackupEvent) + Send + Sync),
) -> Result<FileRunSummary, BackupError> {
    if read_only {
        return Err(BackupError::ReadOnly);
    }
    if !confirmed {
        return Err(BackupError::NeedsConfirmation);
    }
    let started = Instant::now();
    let (text, _) = read(path)?;
    let total = session.dialect().split_statements(&text).len() as u64;
    cancel.attach(session);
    if cancel.is_cancelled() {
        return Err(BackupError::Cancelled);
    }
    // The run asks nothing: a paused run would wait for an answer no one gives.
    let on_error = match on_error {
        OnError::Ask => OnError::Stop,
        other => other,
    };
    let mut stream = session.run_script_with(
        &text,
        ScriptOptions {
            on_error,
            statement_timeout: None,
        },
    );
    let (mut done, mut failed) = (0u64, 0u64);
    let mut last = Instant::now();
    events(BackupEvent::Statements { done: 0, total });
    while let Some(event) = stream.next().await {
        match event {
            CoreEvent::StatementFinished { .. } => {
                done += 1;
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    events(BackupEvent::Statements { done, total });
                }
            }
            CoreEvent::StatementFailed { index, error } => {
                failed += 1;
                if !error.cancelled {
                    events(BackupEvent::Log {
                        line: crate::audit::redact_secrets(&format!(
                            "Statement {} failed: {}",
                            index + 1,
                            error.message
                        )),
                    });
                }
            }
            CoreEvent::ResultStarted { result, .. } => {
                // A file's results are not shown: they are not kept either.
                session.results().remove(result);
            }
            CoreEvent::Notice { notice, .. } => events(BackupEvent::Log {
                line: crate::audit::redact_secrets(&notice.message),
            }),
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    events(BackupEvent::Statements { done, total });
    let summary = FileRunSummary {
        done,
        failed,
        total,
        elapsed: started.elapsed(),
    };
    if cancel.is_cancelled() {
        return Err(BackupError::Cancelled);
    }
    if failed > 0 && on_error == OnError::Stop {
        return Err(BackupError::Failed(format!(
            "the run stopped at a failed statement after {done} of {total}"
        )));
    }
    Ok(summary)
}
