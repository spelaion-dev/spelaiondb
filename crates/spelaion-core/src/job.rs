//! One vocabulary for work that runs (ADR-060): every job of the core (an export, an import, a
//! backup, a restore, a file run, a data search, a column's statistics, a connection test) is
//! stopped by one [`JobCancel`], and is read as one [`JobEnd`]. Each job keeps its own error
//! type for why it failed; how that error reads as an ending is written here, once per job.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use crate::backup::BackupError;
use crate::column_stats::ColumnStatsError;
use crate::data_search::DataSearchSummary;
use crate::export_stream::{ExportError, ExportedFile, TablesError};
use crate::import::{ImportError, ImportSummary};
use crate::open::OpenError;
use crate::session::{Session, SessionCancel};

/// Stops a running job. It is made before the job's session exists (while a tunnel opens or
/// the connection is made), so a cancel that comes early is not lost; the job itself binds it
/// to the session it runs on ([`JobCancel::attach`]), so no caller can forget to.
///
/// A cancel goes through the session ([`Session::cancel`]): it stops the statement running and
/// the run that sent it, whatever its error mode, and the session's next statement waits until
/// the engine has the cancel (SDB-266, SDB-365).
#[derive(Default)]
pub struct JobCancel {
    cancelled: AtomicBool,
    notify: Notify,
    /// The session the job runs on while it is bound, guarded with the flag: a cancel sees the
    /// session, or the job sees the flag, never neither.
    session: std::sync::Mutex<Option<SessionCancel>>,
}

impl JobCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops the job: its statement through its session, if one is bound, and every step after
    /// it. It never waits for the job to end.
    pub async fn cancel(&self) {
        let session = {
            let session = self.session.lock().expect("lock poisoned");
            self.cancelled.store(true, Ordering::SeqCst);
            session.clone()
        };
        self.notify.notify_waiters();
        if let Some(session) = session {
            let _ = session.cancel().await;
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves once cancelled.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// Binds the job to `session` until the returned guard is dropped, or answers `None` when
    /// a cancel already came: the job then stops before it sends anything more.
    ///
    /// A session's run sees the cancels that come after it was requested, so a job that runs
    /// through [`Session::run_script_with`] or [`Session::run_generated`] requests the run
    /// first and binds afterwards: a cancel that came before finds the flag set here, and one
    /// that comes after stops the run.
    pub(crate) fn attach(&self, session: &Session) -> Option<Attached<'_>> {
        let mut bound = self.session.lock().expect("lock poisoned");
        if self.is_cancelled() {
            return None;
        }
        *bound = Some(session.canceller());
        Some(Attached(self))
    }
}

/// A [`JobCancel`] bound to its job's session. Dropping it lets the session go: the cancel
/// does not keep a closed session's connection open.
pub(crate) struct Attached<'a>(&'a JobCancel);

impl Drop for Attached<'_> {
    fn drop(&mut self) {
        *self.0.session.lock().expect("lock poisoned") = None;
    }
}

/// How a job ended, generic over what it returns (`T`) and what it leaves behind when it stops
/// early (`K`: the rows an import committed, the files an export of several tables wrote, or
/// nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd<T, K> {
    /// It ran to the end.
    Done(T),
    /// It stopped at a limit it was given, with what it found until then.
    StoppedAtLimit(T),
    /// It was cancelled. What was already committed or written stays.
    Cancelled(K),
    /// It failed, with why. What was already committed or written stays.
    Failed { message: String, kept: K },
    /// It did not start: something must be asked or changed first.
    Refused(Refusal),
}

/// Why a job did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The connection's type asks before it runs: ask, then start it again confirmed.
    NeedsConfirmation,
    /// The connection's password must be asked first.
    PasswordRequired,
    /// Only a single read may run there; why this is not one.
    NotARead(String),
}

impl<T> From<Result<T, ExportError>> for JobEnd<T, ()> {
    fn from(result: Result<T, ExportError>) -> Self {
        match result {
            Ok(summary) => Self::Done(summary),
            Err(e) => export_failure(e, ()),
        }
    }
}

fn export_failure<T, K>(error: ExportError, kept: K) -> JobEnd<T, K> {
    match error {
        ExportError::Cancelled => JobEnd::Cancelled(kept),
        ExportError::NeedsConfirmation => JobEnd::Refused(Refusal::NeedsConfirmation),
        e @ ExportError::NotARead(_) => JobEnd::Refused(Refusal::NotARead(e.to_string())),
        e @ (ExportError::Table(_)
        | ExportError::Database(_)
        | ExportError::Io { .. }
        | ExportError::Invalid(_)) => JobEnd::Failed {
            message: e.to_string(),
            kept,
        },
    }
}

/// Where an export of several tables stopped: the files finished before, which stay, and the
/// position of the table it stopped at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablesStopped {
    pub files: Vec<ExportedFile>,
    pub table: usize,
}

impl From<Result<Vec<ExportedFile>, TablesError>> for JobEnd<Vec<ExportedFile>, TablesStopped> {
    fn from(result: Result<Vec<ExportedFile>, TablesError>) -> Self {
        match result {
            Ok(files) => Self::Done(files),
            Err(TablesError { done, table, error }) => {
                export_failure(*error, TablesStopped { files: done, table })
            }
        }
    }
}

/// The rows an import committed before it stopped: they stay.
impl From<Result<ImportSummary, ImportError>> for JobEnd<ImportSummary, u64> {
    fn from(result: Result<ImportSummary, ImportError>) -> Self {
        match result {
            Ok(summary) => Self::Done(summary),
            Err(ImportError::Cancelled { committed }) => Self::Cancelled(committed),
            Err(ImportError::NeedsConfirmation) => Self::Refused(Refusal::NeedsConfirmation),
            Err(e @ ImportError::Failed { committed, .. }) => Self::Failed {
                message: e.to_string(),
                kept: committed,
            },
            Err(
                e @ (ImportError::Io { .. }
                | ImportError::Source(_)
                | ImportError::Invalid(_)
                | ImportError::TableExists(_)
                | ImportError::ReadOnly
                | ImportError::Database(_)
                | ImportError::Statement { .. }),
            ) => Self::Failed {
                message: e.to_string(),
                kept: 0,
            },
        }
    }
}

fn backup_failure<T>(error: BackupError) -> JobEnd<T, ()> {
    match error {
        BackupError::Cancelled => JobEnd::Cancelled(()),
        BackupError::NeedsConfirmation => JobEnd::Refused(Refusal::NeedsConfirmation),
        BackupError::Open(OpenError::PasswordRequired { .. }) => {
            JobEnd::Refused(Refusal::PasswordRequired)
        }
        e @ (BackupError::Tool(_)
        | BackupError::ReadOnly
        | BackupError::Open(_)
        | BackupError::Io { .. }
        | BackupError::Invalid(_)
        | BackupError::Failed(_)) => JobEnd::Failed {
            message: e.to_string(),
            kept: (),
        },
    }
}

/// A backup, a restore, a file run, an embedded copy or an import into DuckDB.
impl<T> From<Result<T, BackupError>> for JobEnd<T, ()> {
    fn from(result: Result<T, BackupError>) -> Self {
        result.map_or_else(backup_failure, Self::Done)
    }
}

fn column_stats_failure<T>(error: ColumnStatsError) -> JobEnd<T, ()> {
    match error {
        ColumnStatsError::Cancelled => JobEnd::Cancelled(()),
        e @ ColumnStatsError::NotARead(_) => JobEnd::Refused(Refusal::NotARead(e.to_string())),
        e @ (ColumnStatsError::Invalid(_)
        | ColumnStatsError::Failed(_)
        | ColumnStatsError::Masked) => JobEnd::Failed {
            message: e.to_string(),
            kept: (),
        },
    }
}

impl<T> From<Result<T, ColumnStatsError>> for JobEnd<T, ()> {
    fn from(result: Result<T, ColumnStatsError>) -> Self {
        result.map_or_else(column_stats_failure, Self::Done)
    }
}

/// A data search that ran ends with its summary whichever way it ends: the tables searched
/// before a cancel or the limit keep their hits. (A request it refuses never runs: that is
/// its [`crate::DataSearchError`].)
impl From<DataSearchSummary> for JobEnd<DataSearchSummary, DataSearchSummary> {
    fn from(summary: DataSearchSummary) -> Self {
        if summary.cancelled {
            Self::Cancelled(summary)
        } else if summary.stopped {
            Self::StoppedAtLimit(summary)
        } else {
            Self::Done(summary)
        }
    }
}
