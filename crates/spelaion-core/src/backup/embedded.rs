//! SQLite and DuckDB backups with their own commands (ADR-049), in a session of their own: a
//! SQLite copy with `VACUUM INTO`, a DuckDB folder with `EXPORT DATABASE` and its restore with
//! `IMPORT DATABASE`. No external tool runs; the statements are the core's, with the path
//! written as one string literal.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Local;
use futures_util::StreamExt;
use spelaion_config::{Engine, ResolvedConnection};

use super::process::{BackupCancel, BackupEvent};
use super::{BackupError, io_error, part_path, remove_path, render_file_name, size_of};
use crate::{CoreEvent, Session};

/// DuckDB's export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DuckdbExportFormat {
    #[default]
    Parquet,
    Csv,
}

/// A copy of a SQLite or DuckDB database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedDumpRequest {
    pub folder: PathBuf,
    pub name_pattern: String,
    /// DuckDB only.
    pub duckdb_format: DuckdbExportFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedSummary {
    pub path: PathBuf,
    pub bytes: u64,
    pub elapsed: Duration,
}

/// A string literal for SQLite and DuckDB: quotes doubled; a NUL is refused.
fn literal(path: &Path) -> Result<String, BackupError> {
    let text = path.to_string_lossy();
    if text.contains('\0') {
        return Err(BackupError::Invalid(
            "the path holds a NUL character".into(),
        ));
    }
    Ok(format!("'{}'", text.replace('\'', "''")))
}

fn database_name(resolved: &ResolvedConnection) -> String {
    Path::new(&resolved.params.database)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| resolved.name.clone())
}

/// The file (SQLite) or folder (DuckDB) a copy will be.
pub fn embedded_dump_path(
    resolved: &ResolvedConnection,
    request: &EmbeddedDumpRequest,
) -> Result<PathBuf, BackupError> {
    let extension = match resolved.engine {
        Engine::Sqlite => ".sqlite",
        Engine::Duckdb => "",
        Engine::Postgres | Engine::Mysql | Engine::Mariadb | Engine::Sqlserver => {
            return Err(BackupError::Invalid(format!(
                "{} databases are backed up with a client tool",
                resolved.engine.as_str()
            )));
        }
    };
    if resolved.engine == Engine::Sqlite && resolved.tunnel.is_some() {
        return Err(BackupError::Invalid(
            "this SQLite file is on a server: a copy would be written there, not on this computer"
                .into(),
        ));
    }
    if request.folder.as_os_str().is_empty() {
        return Err(BackupError::Invalid(
            "choose the folder to write the backup to".into(),
        ));
    }
    let name = render_file_name(
        &request.name_pattern,
        &database_name(resolved),
        &resolved.name,
        Local::now(),
    )
    .map_err(BackupError::Invalid)?;
    Ok(request.folder.join(format!("{name}{extension}")))
}

/// Runs one statement the core wrote, stopping it on a cancel.
async fn run_one(
    session: &Session,
    sql: &str,
    cancel: &BackupCancel,
    events: &(dyn Fn(BackupEvent) + Send + Sync),
) -> Result<(), BackupError> {
    cancel.attach(session);
    if cancel.is_cancelled() {
        return Err(BackupError::Cancelled);
    }
    events(BackupEvent::Started {
        command_line: sql.to_string(),
    });
    let mut stream = session.run_generated(sql);
    let mut failure = None;
    while let Some(event) = stream.next().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => failure = Some(error),
            CoreEvent::Notice { notice, .. } => events(BackupEvent::Log {
                line: notice.message,
            }),
            CoreEvent::ResultStarted { result, .. } => {
                session.results().remove(result);
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    if cancel.is_cancelled() {
        return Err(BackupError::Cancelled);
    }
    match failure {
        Some(error) => Err(BackupError::Failed(error.message)),
        None => Ok(()),
    }
}

/// Copies a SQLite or DuckDB database to a file or folder of the user's machine, in `session`
/// (a session of its own). A read: allowed on read-only connections.
pub async fn embedded_dump(
    session: &Session,
    resolved: &ResolvedConnection,
    request: &EmbeddedDumpRequest,
    cancel: &BackupCancel,
    events: &(dyn Fn(BackupEvent) + Send + Sync),
) -> Result<EmbeddedSummary, BackupError> {
    let started = Instant::now();
    let path = embedded_dump_path(resolved, request)?;
    tokio::fs::create_dir_all(&request.folder)
        .await
        .map_err(io_error("create the folder", &request.folder))?;
    if tokio::fs::symlink_metadata(&path).await.is_ok() {
        return Err(BackupError::Invalid(format!(
            "{} already exists: change the file name",
            path.display()
        )));
    }
    let part = part_path(&path);
    remove_path(&part).await;
    let sql = match resolved.engine {
        Engine::Sqlite => format!("VACUUM INTO {}", literal(&part)?),
        Engine::Duckdb => format!(
            "EXPORT DATABASE {} (FORMAT {})",
            literal(&part)?,
            match request.duckdb_format {
                DuckdbExportFormat::Parquet => "parquet",
                DuckdbExportFormat::Csv => "csv",
            }
        ),
        Engine::Postgres | Engine::Mysql | Engine::Mariadb | Engine::Sqlserver => {
            unreachable!("embedded_dump_path refused it")
        }
    };
    match run_one(session, &sql, cancel, events).await {
        Ok(()) => {}
        Err(e) => {
            remove_path(&part).await;
            return Err(e);
        }
    }
    tokio::fs::rename(&part, &path)
        .await
        .map_err(io_error("rename the backup to", &path))?;
    let bytes = size_of(&path).await;
    events(BackupEvent::Progress { bytes, total: None });
    Ok(EmbeddedSummary {
        path,
        bytes,
        elapsed: started.elapsed(),
    })
}

/// Loads a DuckDB export folder into the connection's database (`IMPORT DATABASE`). A write:
/// refused on a read-only connection and unless confirmed.
pub async fn duckdb_import(
    session: &Session,
    resolved: &ResolvedConnection,
    folder: &Path,
    confirmed: bool,
    cancel: &BackupCancel,
    events: &(dyn Fn(BackupEvent) + Send + Sync),
) -> Result<Duration, BackupError> {
    if resolved.engine != Engine::Duckdb {
        return Err(BackupError::Invalid(
            "only DuckDB imports an exported folder".into(),
        ));
    }
    if resolved.params.read_only {
        return Err(BackupError::ReadOnly);
    }
    if !confirmed {
        return Err(BackupError::NeedsConfirmation);
    }
    if !folder.join("schema.sql").is_file() || !folder.join("load.sql").is_file() {
        return Err(BackupError::Invalid(format!(
            "{} is not a DuckDB export (it needs schema.sql and load.sql)",
            folder.display()
        )));
    }
    let started = Instant::now();
    run_one(
        session,
        &format!("IMPORT DATABASE {}", literal(folder)?),
        cancel,
        events,
    )
    .await?;
    Ok(started.elapsed())
}
