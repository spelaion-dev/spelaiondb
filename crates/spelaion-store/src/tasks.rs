//! Saved tasks and their runs (ADR-089): local data that never leaves the machine. The store
//! keeps each task's name, folder and document as text; `spelaion-core`'s `task/` writes the
//! document and checks it again every time it is read back, so nothing here interprets it.
//! Each run is a row written when it starts and again when it ends, by the app or the CLI.

use rusqlite::{OptionalExtension, params};

use crate::{Store, StoreError, now_ms};

/// Longest task name kept, in characters (the core allows the same).
pub const TASK_NAME_MAX: usize = 200;
/// Longest folder label kept, in characters.
pub const TASK_FOLDER_MAX: usize = 400;
/// Largest document kept, in bytes (the core's bounds keep one far smaller).
pub const TASK_DOCUMENT_MAX: usize = 1024 * 1024;
/// Most tasks one store keeps; more is refused rather than silently dropped.
pub const MAX_TASKS: usize = 2_000;
/// Runs kept per task: the oldest go when a new one starts.
pub const TASK_RUNS_KEPT: usize = 100;
/// Largest run log kept, in bytes (the core keeps fewer lines than this holds).
pub const TASK_LOG_MAX: usize = 256 * 1024;
/// Largest run summary kept, in bytes.
pub const TASK_SUMMARY_MAX: usize = 16 * 1024;

pub(crate) const MIGRATION: &str = "
create table tasks (
    id          integer primary key,
    name        text not null collate nocase unique check (length(name) between 1 and 200),
    folder      text not null default '' check (length(folder) <= 400),
    document    text not null check (length(cast(document as blob)) <= 1048576),
    updated_ms  integer not null
);
create table task_runs (
    id          integer primary key,
    task        integer not null,
    origin      text not null check (origin in ('app', 'cli')),
    started_ms  integer not null,
    ended_ms    integer,
    ending      text check (ending in ('succeeded', 'failed', 'cancelled', 'refused')),
    summary     text not null default '' check (length(cast(summary as blob)) <= 16384),
    log         text not null default '' check (length(cast(log as blob)) <= 262144),
    log_omitted integer not null default 0 check (log_omitted >= 0),
    check ((ended_ms is null) = (ending is null))
);
create index task_runs_by_task on task_runs (task, started_ms);
";

/// A saved task as it was written: its document not checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTask {
    pub id: i64,
    pub name: String,
    pub folder: String,
    pub document: String,
    pub updated_ms: i64,
}

/// What saving a task did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSave {
    /// Saved, under this id.
    Saved(i64),
    /// Another task already has the name (names are compared ignoring case).
    NameTaken,
    /// The task to replace is not there any more.
    Missing,
    /// The store already keeps the most tasks it keeps.
    Full,
}

/// Where a run was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOrigin {
    App,
    Cli,
}

impl TaskOrigin {
    fn as_str(self) -> &'static str {
        match self {
            TaskOrigin::App => "app",
            TaskOrigin::Cli => "cli",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "app" => Some(TaskOrigin::App),
            "cli" => Some(TaskOrigin::Cli),
            _ => None,
        }
    }
}

/// How a run ended, as the store keeps it. The core's run state reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskEnding {
    Succeeded,
    Failed,
    Cancelled,
    /// It did not start: a barrier or a missing confirmation refused it.
    Refused,
}

impl TaskEnding {
    fn as_str(self) -> &'static str {
        match self {
            TaskEnding::Succeeded => "succeeded",
            TaskEnding::Failed => "failed",
            TaskEnding::Cancelled => "cancelled",
            TaskEnding::Refused => "refused",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "succeeded" => Some(TaskEnding::Succeeded),
            "failed" => Some(TaskEnding::Failed),
            "cancelled" => Some(TaskEnding::Cancelled),
            "refused" => Some(TaskEnding::Refused),
            _ => None,
        }
    }
}

/// A recorded run, without its log. `ended` is `None` while it runs, or when the program that
/// ran it stopped before it could record its ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTaskRun {
    pub id: i64,
    pub task: i64,
    pub origin: TaskOrigin,
    pub started_ms: i64,
    pub ended: Option<(i64, TaskEnding)>,
    pub summary: String,
}

/// A run's log: its kept lines, and how many more were not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTaskLog {
    pub log: String,
    pub omitted: u64,
}

fn check_names(name: &str, folder: &str) -> Result<(), StoreError> {
    if name.is_empty() || name.chars().count() > TASK_NAME_MAX || name.contains('\0') {
        return Err(StoreError::new(format!(
            "a task's name is 1 to {TASK_NAME_MAX} characters"
        )));
    }
    if folder.chars().count() > TASK_FOLDER_MAX || folder.contains('\0') {
        return Err(StoreError::new(format!(
            "a task's folder is at most {TASK_FOLDER_MAX} characters"
        )));
    }
    Ok(())
}

fn task_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTask> {
    Ok(StoredTask {
        id: r.get(0)?,
        name: r.get(1)?,
        folder: r.get(2)?,
        document: r.get(3)?,
        updated_ms: r.get(4)?,
    })
}

fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTaskRun> {
    let unknown = |column: usize, text: &str| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            format!("unknown value {text:?}").into(),
        )
    };
    let origin: String = r.get(2)?;
    let ended_ms: Option<i64> = r.get(4)?;
    let ending: Option<String> = r.get(5)?;
    let ended = match (ended_ms, ending) {
        (Some(at), Some(ending)) => Some((
            at,
            TaskEnding::parse(&ending).ok_or_else(|| unknown(5, &ending))?,
        )),
        // The table's check keeps both or neither.
        (None, _) | (_, None) => None,
    };
    Ok(StoredTaskRun {
        id: r.get(0)?,
        task: r.get(1)?,
        origin: TaskOrigin::parse(&origin).ok_or_else(|| unknown(2, &origin))?,
        started_ms: r.get(3)?,
        ended,
        summary: r.get(6)?,
    })
}

const TASK_COLUMNS: &str = "id, name, folder, document, updated_ms";
const RUN_COLUMNS: &str = "id, task, origin, started_ms, ended_ms, ending, summary";

impl Store {
    /// Every saved task, by folder and name.
    pub fn tasks(&self) -> Result<Vec<StoredTask>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "select {TASK_COLUMNS} from tasks order by folder collate nocase, name, id"
        ))?;
        let rows = statement.query_map([], task_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One saved task by id, if it is there.
    pub fn task(&self, id: i64) -> Result<Option<StoredTask>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("select {TASK_COLUMNS} from tasks where id = ?1"),
                params![id],
                task_row,
            )
            .optional()?)
    }

    /// One saved task by name, ignoring case, if it is there.
    pub fn task_named(&self, name: &str) -> Result<Option<StoredTask>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("select {TASK_COLUMNS} from tasks where name = ?1"),
                params![name],
                task_row,
            )
            .optional()?)
    }

    /// Saves a task: a new one when `id` is `None`, else over that one (its runs stay). The
    /// name must not be another task's.
    pub fn save_task(
        &self,
        id: Option<i64>,
        name: &str,
        folder: &str,
        document: &str,
    ) -> Result<TaskSave, StoreError> {
        check_names(name, folder)?;
        if document.len() > TASK_DOCUMENT_MAX || document.contains('\0') {
            return Err(StoreError::new("the task is too large to save".into()));
        }
        let transaction = self.conn.unchecked_transaction()?;
        if let Some(id) = id {
            let there: bool = transaction.query_row(
                "select exists(select 1 from tasks where id = ?1)",
                params![id],
                |r| r.get(0),
            )?;
            if !there {
                return Ok(TaskSave::Missing);
            }
        }
        let taken: bool = transaction.query_row(
            "select exists(select 1 from tasks where name = ?1 and id is not ?2)",
            params![name, id],
            |r| r.get(0),
        )?;
        if taken {
            return Ok(TaskSave::NameTaken);
        }
        let saved = match id {
            Some(id) => {
                transaction.execute(
                    "update tasks set name = ?2, folder = ?3, document = ?4, updated_ms = ?5
                     where id = ?1",
                    params![id, name, folder, document, now_ms()],
                )?;
                id
            }
            None => {
                let count: i64 =
                    transaction.query_row("select count(*) from tasks", [], |r| r.get(0))?;
                if count >= MAX_TASKS as i64 {
                    return Ok(TaskSave::Full);
                }
                transaction.execute(
                    "insert into tasks (name, folder, document, updated_ms)
                     values (?1, ?2, ?3, ?4)",
                    params![name, folder, document, now_ms()],
                )?;
                transaction.last_insert_rowid()
            }
        };
        transaction.commit()?;
        Ok(TaskSave::Saved(saved))
    }

    /// Deletes a task and its runs; whether it was there.
    pub fn delete_task(&self, id: i64) -> Result<bool, StoreError> {
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute("delete from task_runs where task = ?1", params![id])?;
        let deleted = transaction.execute("delete from tasks where id = ?1", params![id])? > 0;
        transaction.commit()?;
        Ok(deleted)
    }

    /// Records that a run of `task` starts now, keeping the newest [`TASK_RUNS_KEPT`] runs of
    /// the task (this one among them). Fails when the task is not there.
    pub fn start_task_run(&self, task: i64, origin: TaskOrigin) -> Result<i64, StoreError> {
        let transaction = self.conn.unchecked_transaction()?;
        let there: bool = transaction.query_row(
            "select exists(select 1 from tasks where id = ?1)",
            params![task],
            |r| r.get(0),
        )?;
        if !there {
            return Err(StoreError::new("the task is not there any more".into()));
        }
        transaction.execute(
            "insert into task_runs (task, origin, started_ms) values (?1, ?2, ?3)",
            params![task, origin.as_str(), now_ms()],
        )?;
        let run = transaction.last_insert_rowid();
        transaction.execute(
            "delete from task_runs where task = ?1 and id not in
             (select id from task_runs where task = ?1 order by started_ms desc, id desc limit ?2)",
            params![task, TASK_RUNS_KEPT as i64],
        )?;
        transaction.commit()?;
        Ok(run)
    }

    /// Records how a run ended, with its summary and log (cut to what the store keeps, the cut
    /// counted in `omitted`). Whether the run was there and had no ending yet.
    pub fn end_task_run(
        &self,
        run: i64,
        ending: TaskEnding,
        summary: &str,
        log: &str,
        omitted: u64,
    ) -> Result<bool, StoreError> {
        let summary = cut(summary, TASK_SUMMARY_MAX).0;
        let (log, cut_lines) = cut(log, TASK_LOG_MAX);
        let omitted = i64::try_from(omitted.saturating_add(cut_lines)).unwrap_or(i64::MAX);
        Ok(self.conn.execute(
            "update task_runs set ended_ms = ?2, ending = ?3, summary = ?4, log = ?5,
                 log_omitted = ?6
             where id = ?1 and ended_ms is null",
            params![run, now_ms(), ending.as_str(), summary, log, omitted],
        )? > 0)
    }

    /// Forgets a run that turned out to be a question (a password or a confirmation to ask),
    /// not a run; whether it was there.
    pub fn forget_task_run(&self, run: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "delete from task_runs where id = ?1 and ended_ms is null",
            params![run],
        )? > 0)
    }

    /// The newest `limit` runs of `task`, newest first.
    pub fn task_runs(&self, task: i64, limit: usize) -> Result<Vec<StoredTaskRun>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "select {RUN_COLUMNS} from task_runs where task = ?1
             order by started_ms desc, id desc limit ?2"
        ))?;
        let rows = statement.query_map(
            params![task, i64::try_from(limit).unwrap_or(i64::MAX)],
            run_row,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One run, without its log.
    pub fn task_run(&self, run: i64) -> Result<Option<StoredTaskRun>, StoreError> {
        Ok(self
            .conn
            .query_row(
                &format!("select {RUN_COLUMNS} from task_runs where id = ?1"),
                params![run],
                run_row,
            )
            .optional()?)
    }

    /// A run's log, if the run is there.
    pub fn task_run_log(&self, run: i64) -> Result<Option<StoredTaskLog>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "select log, log_omitted from task_runs where id = ?1",
                params![run],
                |r| {
                    Ok(StoredTaskLog {
                        log: r.get(0)?,
                        omitted: r.get::<_, i64>(1)?.max(0) as u64,
                    })
                },
            )
            .optional()?)
    }
}

/// `text` cut at a line break to at most `max` bytes, and how many lines were left out.
fn cut(text: &str, max: usize) -> (&str, u64) {
    if text.len() <= max {
        return (text, 0);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = match text[..end].rfind('\n') {
        Some(at) => &text[..at],
        None => &text[..end],
    };
    let left = text[kept.len()..].trim_start_matches('\n');
    (kept, left.lines().count() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIGRATIONS;
    use crate::tests::temp;

    #[test]
    fn tasks_are_saved_by_a_name_unique_ignoring_case() {
        let path = temp("tasks");
        let store = Store::open(&path).unwrap();
        let TaskSave::Saved(first) = store
            .save_task(None, "Weekly orders", "Exports", "{\"v\":1}")
            .unwrap()
        else {
            panic!("not saved");
        };
        assert_eq!(
            store.save_task(None, "weekly ORDERS", "", "{}").unwrap(),
            TaskSave::NameTaken
        );
        let TaskSave::Saved(second) = store.save_task(None, "Nightly dump", "", "{}").unwrap()
        else {
            panic!("not saved");
        };
        assert_eq!(
            store
                .save_task(Some(second), "Weekly orders", "", "{}")
                .unwrap(),
            TaskSave::NameTaken
        );
        assert_eq!(
            store
                .save_task(Some(first), "Weekly orders", "Exports/Sales", "{\"v\":2}")
                .unwrap(),
            TaskSave::Saved(first)
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        let task = store.task_named("WEEKLY orders").unwrap().unwrap();
        assert_eq!(task.id, first);
        assert_eq!(task.folder, "Exports/Sales");
        assert_eq!(task.document, "{\"v\":2}");
        let listed: Vec<_> = store.tasks().unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(listed, ["Nightly dump", "Weekly orders"]);
        assert!(store.delete_task(first).unwrap());
        assert!(!store.delete_task(first).unwrap());
        assert_eq!(
            store.save_task(Some(first), "x", "", "{}").unwrap(),
            TaskSave::Missing
        );
        assert!(store.save_task(None, "", "", "{}").is_err());
        assert!(
            store
                .save_task(None, "x", &"f".repeat(TASK_FOLDER_MAX + 1), "{}")
                .is_err()
        );
        assert!(
            store
                .save_task(None, "x", "", &"x".repeat(TASK_DOCUMENT_MAX + 1))
                .is_err()
        );
    }

    #[test]
    fn runs_are_recorded_when_they_start_and_end_and_the_newest_are_kept() {
        let store = Store::open(&temp("task-runs")).unwrap();
        let TaskSave::Saved(task) = store.save_task(None, "t", "", "{}").unwrap() else {
            panic!("not saved");
        };
        let from_app = store.start_task_run(task, TaskOrigin::App).unwrap();
        let from_cli = store.start_task_run(task, TaskOrigin::Cli).unwrap();
        assert!(
            store
                .end_task_run(from_app, TaskEnding::Succeeded, "3 rows", "a\nb", 0)
                .unwrap()
        );
        // An ending is recorded once.
        assert!(
            !store
                .end_task_run(from_app, TaskEnding::Failed, "", "", 0)
                .unwrap()
        );
        let runs = store.task_runs(task, 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, from_cli);
        assert_eq!(runs[0].origin, TaskOrigin::Cli);
        assert_eq!(runs[0].ended, None);
        assert_eq!(runs[1].origin, TaskOrigin::App);
        assert_eq!(runs[1].ended.map(|e| e.1), Some(TaskEnding::Succeeded));
        assert_eq!(runs[1].summary, "3 rows");
        assert_eq!(
            store.task_run_log(from_app).unwrap().unwrap(),
            StoredTaskLog {
                log: "a\nb".into(),
                omitted: 0
            }
        );
        // A question is not a run.
        assert!(store.forget_task_run(from_cli).unwrap());
        assert_eq!(store.task_runs(task, 10).unwrap().len(), 1);
        for _ in 0..TASK_RUNS_KEPT + 5 {
            store.start_task_run(task, TaskOrigin::Cli).unwrap();
        }
        assert_eq!(store.task_runs(task, 1000).unwrap().len(), TASK_RUNS_KEPT);
        assert!(store.task_run(from_app).unwrap().is_none());
        // Deleting the task deletes its runs; a run of a task that is gone does not start.
        store.delete_task(task).unwrap();
        assert!(store.task_runs(task, 1000).unwrap().is_empty());
        assert!(store.start_task_run(task, TaskOrigin::App).is_err());
    }

    #[test]
    fn a_long_log_is_cut_at_a_line_and_the_rest_counted() {
        let store = Store::open(&temp("task-log")).unwrap();
        let TaskSave::Saved(task) = store.save_task(None, "t", "", "{}").unwrap() else {
            panic!("not saved");
        };
        let run = store.start_task_run(task, TaskOrigin::App).unwrap();
        let line = "x".repeat(99);
        let log = vec![line.as_str(); 4000].join("\n");
        store
            .end_task_run(run, TaskEnding::Failed, "", &log, 7)
            .unwrap();
        let kept = store.task_run_log(run).unwrap().unwrap();
        assert!(kept.log.len() <= TASK_LOG_MAX);
        assert!(kept.log.lines().all(|l| l == line));
        assert_eq!(kept.log.lines().count() as u64 + kept.omitted, 4000 + 7);
    }

    #[test]
    fn the_migration_keeps_what_the_store_already_had() {
        let path = temp("tasks-migration");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let before = MIGRATIONS
                .iter()
                .position(|migration| *migration == MIGRATION)
                .unwrap();
            for migration in &MIGRATIONS[..before] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", before as i64)
                .unwrap();
            conn.execute_batch("insert into personal_settings values('language','es');")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.personal_setting("language").unwrap().as_deref(),
            Some("es")
        );
        assert!(store.tasks().unwrap().is_empty());
    }
}
