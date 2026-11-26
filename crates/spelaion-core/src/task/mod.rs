//! Saved tasks (ADR-089): an export, an import, a dump or a restore, or the run of a `.sql`
//! file, saved with its options under a name and run again from the app or from the CLI
//! (`spelaiondb task run NAME`). Nothing runs on its own: a task runs when a person starts it,
//! or when their own scheduler runs the CLI.
//!
//! A task is local data in the store (`spelaion-store`'s `tasks.rs`). This module writes its
//! document from the core's own request types and checks it when it is saved and every time
//! it is read back (`document.rs`); `run.rs` resolves its connection the same way for the app
//! and the CLI, decides whether a run needs a yes exactly where the run by hand would ask,
//! runs it in a session of its own and records the run with a redacted, bounded log.

mod document;
mod run;

#[cfg(test)]
mod tests;

pub use document::{
    ExportSettings, ExportSource, ExportTask, ImportTask, RestoreTask, RunFileTask, TableName,
    TaskConnection, TaskDocument, TaskDuckdbImport, TaskDumpRequest, TaskEmbeddedDump, TaskKind,
    TaskRequest, TaskTools,
};
pub use run::{
    Questions, TaskEnv, TaskEvent, TaskOutcome, TaskPlan, TaskProgress, resolve_connection,
    run_task,
};

use spelaion_config::{ResolvedConnection, check_plain_text};
use spelaion_driver_api::Secret;
use spelaion_store::{
    Store, StoreError, StoredTaskRun, TASK_FOLDER_MAX, TASK_NAME_MAX, TaskEnding, TaskOrigin,
    TaskSave,
};

use crate::Redactor;
use crate::conn_test::learn_connection_secrets;

/// Why a task could not be saved, read or run.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// The name, the folder or the document is not one a task may have.
    #[error("{0}")]
    Invalid(String),
    /// The document holds one of the connection's secrets (ADR-089 point 2).
    #[error(
        "{0} holds what looks like a password or another secret: a task keeps options, never secrets"
    )]
    Secret(String),
    /// Another task has the name.
    #[error("a task named {0:?} exists already")]
    NameTaken(String),
    #[error("there is no task named {0:?}")]
    NotFound(String),
    #[error(
        "the store keeps at most {} tasks: delete some first",
        spelaion_store::MAX_TASKS
    )]
    Full,
    /// A saved document this version cannot use: it runs nothing.
    #[error("this task cannot be read: {0}")]
    Unreadable(String),
    #[error("{0}")]
    Store(#[from] StoreError),
}

/// A task as it is listed: its document read and checked, or why it could not be.
#[derive(Debug, Clone)]
pub struct SavedTask {
    pub id: i64,
    pub name: String,
    pub folder: String,
    pub updated_ms: i64,
    pub document: Result<TaskDocument, String>,
}

/// How a recorded run stands (ADR-060: one state type, owned by the core).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRunState {
    /// No ending was recorded: it is still running, or the program that ran it stopped.
    Unfinished,
    Succeeded,
    Failed,
    Cancelled,
    /// A barrier or a missing yes refused it before anything ran.
    Refused,
}

/// A recorded run, without its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRun {
    pub id: i64,
    pub origin: TaskOrigin,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub state: TaskRunState,
    pub summary: String,
}

impl From<StoredTaskRun> for TaskRun {
    fn from(run: StoredTaskRun) -> Self {
        let (ended_ms, state) = match run.ended {
            None => (None, TaskRunState::Unfinished),
            Some((at, ending)) => (
                Some(at),
                match ending {
                    TaskEnding::Succeeded => TaskRunState::Succeeded,
                    TaskEnding::Failed => TaskRunState::Failed,
                    TaskEnding::Cancelled => TaskRunState::Cancelled,
                    TaskEnding::Refused => TaskRunState::Refused,
                },
            ),
        };
        Self {
            id: run.id,
            origin: run.origin,
            started_ms: run.started_ms,
            ended_ms,
            state,
            summary: run.summary,
        }
    }
}

/// A moment of a run as the CLI and the logs show it: the local date and time to the second.
pub fn format_time(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(at) | chrono::LocalResult::Ambiguous(at, _) => {
            at.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        chrono::LocalResult::None => ms.to_string(),
    }
}

/// A task's name: one line of plain text, 1 to [`TASK_NAME_MAX`] characters, trimmed.
pub fn check_name(name: &str) -> Result<String, TaskError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(TaskError::Invalid("a task needs a name".into()));
    }
    if name.contains(['\n', '\r', '\t']) || check_plain_text(name, TASK_NAME_MAX).is_err() {
        return Err(TaskError::Invalid(format!(
            "a task's name is one line of at most {TASK_NAME_MAX} characters, without hidden characters"
        )));
    }
    Ok(name.to_string())
}

/// A task's folder: `/`-separated parts, each trimmed, empty ones left out; empty for none.
pub fn check_folder(folder: &str) -> Result<String, TaskError> {
    let folder = folder
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if folder.contains(['\n', '\r', '\t']) || check_plain_text(&folder, TASK_FOLDER_MAX).is_err() {
        return Err(TaskError::Invalid(format!(
            "a task's folder is one line of at most {TASK_FOLDER_MAX} characters, without hidden characters"
        )));
    }
    Ok(folder)
}

/// The judge of point 2 of ADR-089: the secrets the configuration resolved for `resolved`
/// (its password, its tunnel's and proxy's) and the password typed for it, if any. A password
/// that is also the connection's user or database name is not one: the configuration writes
/// those as plain text, and every query or path that names them would be refused.
pub fn secrets_of(resolved: &ResolvedConnection, typed: Option<&Secret>) -> Redactor {
    let redactor = Redactor::new();
    learn_connection_secrets(&redactor, resolved);
    if let Some(password) = typed {
        redactor.add(password.expose());
    }
    redactor.forget(&resolved.params.user);
    redactor.forget(&resolved.params.database);
    redactor
}

/// Saves `document` as the task `name` in `folder`: a new task when `id` is `None`, else over
/// that one. Checked first (the document's own checks, then that none of its text holds one of
/// `secrets`); a name taken by another task is refused.
pub fn save_task(
    store: &Store,
    id: Option<i64>,
    name: &str,
    folder: &str,
    document: &TaskDocument,
    secrets: &Redactor,
) -> Result<i64, TaskError> {
    let name = check_name(name)?;
    let folder = check_folder(folder)?;
    document.check()?;
    document.check_secrets(secrets)?;
    match store.save_task(id, &name, &folder, &document.to_json()?)? {
        TaskSave::Saved(id) => Ok(id),
        TaskSave::NameTaken => Err(TaskError::NameTaken(name)),
        TaskSave::Missing => Err(TaskError::NotFound(name)),
        TaskSave::Full => Err(TaskError::Full),
    }
}

/// Renames a task or moves it to another folder, keeping its document and its runs.
pub fn rename_task(store: &Store, id: i64, name: &str, folder: &str) -> Result<(), TaskError> {
    let name = check_name(name)?;
    let folder = check_folder(folder)?;
    let task = store
        .task(id)?
        .ok_or_else(|| TaskError::NotFound(name.clone()))?;
    match store.save_task(Some(id), &name, &folder, &task.document)? {
        TaskSave::Saved(_) => Ok(()),
        TaskSave::NameTaken => Err(TaskError::NameTaken(name)),
        TaskSave::Missing => Err(TaskError::NotFound(name)),
        TaskSave::Full => Err(TaskError::Full),
    }
}

/// A copy of a task under the first free name `<name> (copy)`, `<name> (copy 2)`…, in the same
/// folder, without its runs. Its document is checked again before it is copied.
pub fn duplicate_task(store: &Store, id: i64) -> Result<i64, TaskError> {
    let task = store
        .task(id)?
        .ok_or_else(|| TaskError::NotFound(id.to_string()))?;
    TaskDocument::from_json(&task.document).map_err(TaskError::Unreadable)?;
    for n in 1..1000 {
        let suffix = if n == 1 {
            " (copy)".to_string()
        } else {
            format!(" (copy {n})")
        };
        let mut base: String = task.name.clone();
        while base.chars().count() + suffix.chars().count() > TASK_NAME_MAX {
            base.pop();
        }
        let name = format!("{}{suffix}", base.trim_end());
        match store.save_task(None, &name, &task.folder, &task.document)? {
            TaskSave::Saved(id) => return Ok(id),
            TaskSave::NameTaken => continue,
            TaskSave::Missing => return Err(TaskError::NotFound(task.name)),
            TaskSave::Full => return Err(TaskError::Full),
        }
    }
    Err(TaskError::Invalid(
        "no free name for the copy: rename some copies first".into(),
    ))
}

fn saved(task: spelaion_store::StoredTask) -> SavedTask {
    SavedTask {
        document: TaskDocument::from_json(&task.document),
        id: task.id,
        name: task.name,
        folder: task.folder,
        updated_ms: task.updated_ms,
    }
}

/// Every saved task, by folder and name, each document read and checked.
pub fn list_tasks(store: &Store) -> Result<Vec<SavedTask>, TaskError> {
    Ok(store.tasks()?.into_iter().map(saved).collect())
}

/// One task by id.
pub fn task(store: &Store, id: i64) -> Result<SavedTask, TaskError> {
    store
        .task(id)?
        .map(saved)
        .ok_or_else(|| TaskError::NotFound(id.to_string()))
}

/// One task by name, ignoring case (the CLI's `NAME`).
pub fn task_named(store: &Store, name: &str) -> Result<SavedTask, TaskError> {
    store
        .task_named(name.trim())?
        .map(saved)
        .ok_or_else(|| TaskError::NotFound(name.trim().to_string()))
}

/// The newest `limit` runs of a task, newest first.
pub fn task_runs(store: &Store, id: i64, limit: usize) -> Result<Vec<TaskRun>, TaskError> {
    Ok(store
        .task_runs(id, limit)?
        .into_iter()
        .map(TaskRun::from)
        .collect())
}

/// A run's kept log and how many lines were not kept.
pub fn task_run_log(store: &Store, run: i64) -> Result<(String, u64), TaskError> {
    let log = store
        .task_run_log(run)?
        .ok_or_else(|| TaskError::NotFound(format!("run {run}")))?;
    Ok((log.log, log.omitted))
}
