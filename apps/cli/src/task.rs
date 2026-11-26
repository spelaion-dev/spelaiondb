//! `spelaiondb task`: lists, runs and shows the history of the tasks saved in the app
//! (ADR-089). A run asks nobody: a connection that needs a person fails, and a task that needs
//! a yes runs only with `--yes`, the confirmation written in the command itself. Nothing here
//! schedules anything; the user's own scheduler calls `task run`.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use clap::Subcommand;
use spelaion_core::task::{
    self, Questions, TaskEnv, TaskEvent, TaskOutcome, TaskRunState, format_time,
};
use spelaion_core::{Engines, JobCancel, TunnelPool};
use spelaion_store::{Store, TaskOrigin};

#[derive(Subcommand)]
pub enum TaskCommand {
    /// Lists the tasks saved in the app, by folder, with their last run.
    List,
    /// Runs a saved task now, as the app's Run does. A task that the run by hand would ask
    /// about (an import that replaces a table, a restore, a file run, any run on a connection
    /// whose type confirms execution) runs only with --yes. Exits with 1 unless it succeeded.
    Run {
        /// The task's name (case does not matter).
        name: String,
        /// Confirms a task that asks before it runs.
        #[arg(long)]
        yes: bool,
    },
    /// Shows a task's latest runs, from the app and from the command line.
    History {
        name: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Prints each run's log under it.
        #[arg(long)]
        log: bool,
    },
}

fn store(user_dir: &Path) -> Result<Store> {
    Ok(Store::open(&spelaion_config::state_db(user_dir))?)
}

fn state_word(state: TaskRunState) -> &'static str {
    match state {
        TaskRunState::Unfinished => "unfinished",
        TaskRunState::Succeeded => "succeeded",
        TaskRunState::Failed => "FAILED",
        TaskRunState::Cancelled => "cancelled",
        TaskRunState::Refused => "REFUSED",
    }
}

fn origin_word(origin: TaskOrigin) -> &'static str {
    match origin {
        TaskOrigin::App => "app",
        TaskOrigin::Cli => "cli",
    }
}

pub async fn command(user_dir: &Path, command: TaskCommand) -> Result<()> {
    match command {
        TaskCommand::List => list(user_dir),
        TaskCommand::Run { name, yes } => run(user_dir, &name, yes).await,
        TaskCommand::History { name, limit, log } => history(user_dir, &name, limit, log),
    }
}

fn list(user_dir: &Path) -> Result<()> {
    let store = store(user_dir)?;
    let tasks = task::list_tasks(&store)?;
    if tasks.is_empty() {
        println!("No saved tasks: save one from an export, import or backup dialog in the app.");
    }
    for saved in tasks {
        let folder = if saved.folder.is_empty() {
            String::new()
        } else {
            format!("{}/", saved.folder)
        };
        let what = match &saved.document {
            Ok(document) => format!(
                "{} on {}",
                document.request.kind().label(),
                document.connection.id
            ),
            Err(reason) => format!("unreadable: {reason}"),
        };
        let last = match task::task_runs(&store, saved.id, 1)?.first() {
            Some(run) => format!(
                "last run {} ({}, {})",
                format_time(run.started_ms),
                origin_word(run.origin),
                state_word(run.state)
            ),
            None => "never run".into(),
        };
        println!("{folder}{} — {what} — {last}", saved.name);
    }
    Ok(())
}

fn history(user_dir: &Path, name: &str, limit: usize, log: bool) -> Result<()> {
    let store = store(user_dir)?;
    let saved = task::task_named(&store, name)?;
    let runs = task::task_runs(&store, saved.id, limit)?;
    if runs.is_empty() {
        println!("{} has not run yet.", saved.name);
    }
    for run in runs {
        println!(
            "[{}] {} · {}: {}",
            format_time(run.started_ms),
            origin_word(run.origin),
            state_word(run.state),
            run.summary
        );
        if log {
            let (text, omitted) = task::task_run_log(&store, run.id)?;
            for line in text.lines() {
                println!("    {line}");
            }
            if omitted > 0 {
                println!("    … {omitted} more lines not kept");
            }
        }
    }
    Ok(())
}

async fn run(user_dir: &Path, name: &str, yes: bool) -> Result<()> {
    let id = {
        let store = store(user_dir)?;
        task::task_named(&store, name)?.id
    };
    let env = TaskEnv {
        user_dir: user_dir.to_path_buf(),
        // The plugins the app installed, under the last catalogue it accepted (ADR-055).
        engines: Arc::new(Engines::builtin_with(
            &spelaion_core::plugins::installed::PluginStore::in_user_dir(user_dir),
        )),
        tunnels: Arc::new(TunnelPool::new()),
        interaction: Arc::new(spelaion_ssh::NonInteractive),
        password: None,
        questions: Questions::Refuse,
        tools: None,
    };
    let cancel = Arc::new(JobCancel::new());
    // Ctrl+C stops the run as the app's Cancel would: an export leaves no file.
    let on_interrupt = cancel.clone();
    let interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("cancelling…");
            on_interrupt.cancel().await;
        }
    });
    let events = |event: TaskEvent| match event {
        TaskEvent::Log(line) => eprintln!("{line}"),
        TaskEvent::Progress(_) => {}
    };
    let outcome = task::run_task(id, TaskOrigin::Cli, &env, yes, &cancel, &events).await;
    interrupt.abort();
    match outcome? {
        TaskOutcome::Ran {
            state: TaskRunState::Succeeded,
            summary,
            ..
        } => {
            println!("{summary}");
            Ok(())
        }
        TaskOutcome::Ran { state, message, .. } => Err(anyhow!(
            "the task {}: {}",
            state_word(state).to_lowercase(),
            message.unwrap_or_default()
        )),
        // `Questions::Refuse` records these as a refused or failed run instead.
        TaskOutcome::NeedsConfirmation(_) | TaskOutcome::PasswordRequired { .. } => {
            Err(anyhow!("the task asked a question nobody can answer here"))
        }
    }
}
