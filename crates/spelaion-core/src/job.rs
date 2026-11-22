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

/// The jobs one program runs, each under an id the registry makes (SDB-369, ADR-060). Starting
/// a job gives a [`JobHandle`]; the job is listed while its handle lives and gone when it is
/// dropped, however its work ends (a panic too). A cancel names the kind of job it stops: an id
/// of another kind is refused, so a Cancel never reaches a different job.
///
/// `K` is what the program calls its kinds of job (the desktop's export, import, backup…).
pub struct JobRegistry<K> {
    jobs: std::sync::Arc<std::sync::Mutex<Registered<K>>>,
}

struct Registered<K> {
    /// The id the next job gets. Ids are never handed out twice while a job holds one.
    next: u32,
    running: std::collections::HashMap<u32, (K, std::sync::Arc<JobCancel>)>,
}

impl<K> Default for JobRegistry<K> {
    fn default() -> Self {
        Self {
            jobs: std::sync::Arc::new(std::sync::Mutex::new(Registered {
                next: 1,
                running: std::collections::HashMap::new(),
            })),
        }
    }
}

/// Why a cancel was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CancelRefused<K: std::fmt::Debug> {
    /// The id is a job of another kind.
    #[error("job {id} is a {running:?}, not a {asked:?}: it was not cancelled")]
    OtherKind { id: u32, asked: K, running: K },
}

impl<K: Copy + Eq + std::fmt::Debug> JobRegistry<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a job of `kind`: a new id, never one a running job holds, and its cancel.
    pub fn start(&self, kind: K) -> JobHandle<K> {
        let cancel = std::sync::Arc::new(JobCancel::new());
        let mut jobs = self.jobs.lock().expect("lock poisoned");
        let id = loop {
            let id = jobs.next;
            jobs.next = jobs.next.wrapping_add(1).max(1);
            if !jobs.running.contains_key(&id) {
                break id;
            }
        };
        jobs.running.insert(id, (kind, cancel.clone()));
        JobHandle {
            id,
            kind,
            cancel,
            jobs: std::sync::Arc::downgrade(&self.jobs),
        }
    }

    /// Cancels the job `id` if it is a `kind` still running. One that has ended is not an
    /// error: there is nothing left to stop.
    pub async fn cancel(&self, id: u32, kind: K) -> Result<(), CancelRefused<K>> {
        let cancel = match self.jobs.lock().expect("lock poisoned").running.get(&id) {
            None => return Ok(()),
            Some((running, _)) if *running != kind => {
                return Err(CancelRefused::OtherKind {
                    id,
                    asked: kind,
                    running: *running,
                });
            }
            Some((_, cancel)) => cancel.clone(),
        };
        cancel.cancel().await;
        Ok(())
    }

    /// The kind of the job `id`, while it runs.
    pub fn kind_of(&self, id: u32) -> Option<K> {
        self.jobs
            .lock()
            .expect("lock poisoned")
            .running
            .get(&id)
            .map(|(kind, _)| *kind)
    }

    /// How many jobs run now.
    pub fn running(&self) -> usize {
        self.jobs.lock().expect("lock poisoned").running.len()
    }
}

/// A running job of a [`JobRegistry`]: its id, its kind and its cancel. Dropping it removes the
/// job from the registry.
pub struct JobHandle<K> {
    id: u32,
    kind: K,
    cancel: std::sync::Arc<JobCancel>,
    jobs: std::sync::Weak<std::sync::Mutex<Registered<K>>>,
}

impl<K: Copy> JobHandle<K> {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn kind(&self) -> K {
        self.kind
    }

    /// What stops the job: the core's jobs take it as they are.
    pub fn cancel(&self) -> &std::sync::Arc<JobCancel> {
        &self.cancel
    }
}

impl<K> Drop for JobHandle<K> {
    fn drop(&mut self) {
        if let Some(jobs) = self.jobs.upgrade() {
            // Never panics while it unwinds: a poisoned registry is still cleaned.
            let mut jobs = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.running.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Export,
        Import,
    }

    #[test]
    fn two_running_jobs_never_share_an_id() {
        let registry = JobRegistry::new();
        let handles: Vec<_> = (0..100).map(|_| registry.start(Kind::Export)).collect();
        let mut ids: Vec<u32> = handles.iter().map(JobHandle::id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 100);
        assert_eq!(registry.running(), 100);
        drop(handles);
        assert_eq!(registry.running(), 0);
    }

    #[test]
    fn an_id_still_held_is_skipped_when_the_numbers_wrap() {
        let registry = JobRegistry::new();
        let held = registry.start(Kind::Export);
        // As if every other number had been handed out since.
        registry.jobs.lock().unwrap().next = held.id();
        let next = registry.start(Kind::Import);
        assert_ne!(next.id(), held.id());
        registry.jobs.lock().unwrap().next = u32::MAX;
        let last = registry.start(Kind::Import);
        let wrapped = registry.start(Kind::Import);
        assert_eq!(last.id(), u32::MAX);
        assert_ne!(wrapped.id(), 0, "0 is never an id");
        assert_ne!(wrapped.id(), held.id());
    }

    #[tokio::test]
    async fn a_cancel_with_another_kinds_id_is_refused_and_stops_nothing() {
        let registry = JobRegistry::new();
        let export = registry.start(Kind::Export);
        let refused = registry.cancel(export.id(), Kind::Import).await;
        assert_eq!(
            refused,
            Err(CancelRefused::OtherKind {
                id: export.id(),
                asked: Kind::Import,
                running: Kind::Export,
            })
        );
        assert!(!export.cancel().is_cancelled());
        registry.cancel(export.id(), Kind::Export).await.unwrap();
        assert!(export.cancel().is_cancelled());
        // A job that has ended: nothing to stop, and not an error.
        let id = export.id();
        drop(export);
        assert_eq!(registry.cancel(id, Kind::Export).await, Ok(()));
        assert_eq!(registry.kind_of(id), None);
    }

    #[tokio::test]
    async fn a_job_that_panics_leaves_no_entry() {
        let registry = std::sync::Arc::new(JobRegistry::new());
        let job = registry.start(Kind::Import);
        let id = job.id();
        let task = tokio::spawn(async move {
            let _job = job;
            panic!("the command failed halfway");
        });
        assert!(task.await.is_err());
        assert_eq!(registry.kind_of(id), None);
        assert_eq!(registry.running(), 0);
    }
}
