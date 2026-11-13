//! Running one tool (ADR-049): an argument list, never a shell; the password only in its
//! environment; its standard output written to a file or logged, a file fed to its standard
//! input, every log line redacted; stopped by a cancel.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use spelaion_driver_api::Secret;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

use super::definer::strip_definer;
use crate::Redactor;

/// A GUI app must not flash a console window for each tool.
pub(crate) fn no_window(command: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Stops a running backup, restore or file run. Created before anything starts, so a cancel
/// that comes while the tunnel is still opening is not lost.
#[derive(Default)]
pub struct BackupCancel {
    cancelled: AtomicBool,
    notify: Notify,
    /// The session whose statement a cancel also stops (SQLite, DuckDB and file runs).
    session: std::sync::Mutex<Option<std::sync::Arc<dyn spelaion_driver_api::CancelHandle>>>,
}

impl BackupCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        let handle = self.session.lock().expect("lock poisoned").clone();
        if let Some(handle) = handle {
            let _ = handle.cancel().await;
        }
    }

    /// The session whose statement a cancel also stops.
    pub(crate) fn attach(&self, session: &crate::Session) {
        *self.session.lock().expect("lock poisoned") = Some(session.connection().cancel_handle());
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves once cancelled.
    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// What a run reports as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupEvent {
    /// The command started, as it runs (the password is not part of it).
    Started { command_line: String },
    /// A line the tool printed, or a note of ours, already redacted.
    Log { line: String },
    /// Bytes written to the backup, or read from the restored file of `total`.
    Progress { bytes: u64, total: Option<u64> },
    /// Statements run of a file's total (running an `.sql` file).
    Statements { done: u64, total: u64 },
}

pub(crate) type EventSink<'a> = &'a (dyn Fn(BackupEvent) + Send + Sync);

/// Where the tool's standard output goes.
pub(crate) enum Output {
    /// Each line to the log.
    Log,
    /// To this file, with `DEFINER=…` clauses removed when asked.
    File { path: PathBuf, strip_definer: bool },
}

/// What feeds the tool's standard input.
pub(crate) enum Input {
    None,
    File(PathBuf),
}

pub(crate) struct ToolRun {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Variables set for the tool only (the password).
    pub env: Vec<(&'static str, Secret)>,
    pub input: Input,
    pub output: Output,
}

/// How a tool ended.
#[derive(Debug)]
pub(crate) enum RunEnd {
    Done,
    Failed { message: String },
    Cancelled,
}

/// The longest log line kept.
const LINE_MAX: usize = 4000;
/// The tool's last lines, for the failure message.
const TAIL_LINES: usize = 12;
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// The command line shown to the user: the program and each argument, quoted where needed.
pub(crate) fn command_line(program: &Path, args: &[String]) -> String {
    let quote = |s: &str| {
        if !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_=./:,@+%".contains(c))
        {
            s.to_string()
        } else {
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
        }
    };
    std::iter::once(quote(&program.display().to_string()))
        .chain(args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs the tool until it exits or the run is cancelled.
pub(crate) async fn run_tool(
    run: ToolRun,
    redactor: &Redactor,
    cancel: &BackupCancel,
    events: EventSink<'_>,
) -> Result<RunEnd, std::io::Error> {
    let mut command = Command::new(&run.program);
    command
        .args(&run.args)
        .stdin(match run.input {
            Input::None => Stdio::null(),
            Input::File(_) => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, value) in &run.env {
        command.env(name, value.expose());
    }
    no_window(&mut command);
    events(BackupEvent::Started {
        command_line: redactor.redact(&command_line(&run.program, &run.args)),
    });
    if cancel.is_cancelled() {
        return Ok(RunEnd::Cancelled);
    }
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdin = child.stdin.take();

    let tail = std::sync::Mutex::new(std::collections::VecDeque::<String>::new());
    let log = |line: &str| {
        let mut line = redactor.redact(line.trim_end_matches(['\r', '\n']));
        if line.len() > LINE_MAX {
            let mut cut = LINE_MAX;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
            line.push('…');
        }
        if line.trim().is_empty() {
            return;
        }
        {
            let mut tail = tail.lock().expect("lock poisoned");
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line.clone());
        }
        events(BackupEvent::Log { line });
    };

    let output_progress = matches!(run.output, Output::File { .. });
    let work = async {
        let stderr_task = log_lines(stderr, &log);
        let stdout_task = async {
            match &run.output {
                Output::Log => log_lines(stdout, &log).await.map(|()| 0),
                Output::File {
                    path,
                    strip_definer,
                } => write_output(stdout, path, *strip_definer, events).await,
            }
        };
        let stdin_task = async {
            match (&run.input, stdin) {
                (Input::File(path), Some(stdin)) => {
                    feed_input(path, stdin, events, !output_progress).await
                }
                _ => Ok(()),
            }
        };
        let (stderr_done, written, fed, status) =
            tokio::join!(stderr_task, stdout_task, stdin_task, child.wait());
        (stderr_done, written, fed, status)
    };
    let finished = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        done = work => Some(done),
    };
    let Some((stderr_done, written, fed, status)) = finished else {
        // Killed and waited for, so its files are closed before the caller removes them.
        let _ = child.kill().await;
        return Ok(RunEnd::Cancelled);
    };
    let status = status?;
    stderr_done?;
    if !status.success() {
        let code = status
            .code()
            .map(|c| format!(" (exit code {c})"))
            .unwrap_or_default();
        let tail = tail
            .lock()
            .expect("lock poisoned")
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        return Ok(RunEnd::Failed {
            message: if tail.is_empty() {
                format!("the tool failed{code}")
            } else {
                format!("the tool failed{code}: {tail}")
            },
        });
    }
    written?;
    // A tool that stopped reading its input early but exited 0 did not restore all of it.
    if let Err(e) = fed {
        return Ok(RunEnd::Failed {
            message: format!("could not feed the file to the tool: {e}"),
        });
    }
    Ok(RunEnd::Done)
}

async fn log_lines(
    stream: impl AsyncRead + Unpin,
    log: &(dyn Fn(&str) + Sync),
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer).await? == 0 {
            return Ok(());
        }
        log(&String::from_utf8_lossy(&buffer));
    }
}

/// Copies the tool's output to `path`, reporting bytes written.
async fn write_output(
    stream: impl AsyncRead + Unpin,
    path: &Path,
    definer: bool,
    events: EventSink<'_>,
) -> std::io::Result<u64> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut reader = BufReader::with_capacity(256 * 1024, stream);
    let mut written = 0u64;
    let mut last = Instant::now();
    let mut buffer = Vec::with_capacity(256 * 1024);
    loop {
        buffer.clear();
        let n = if definer {
            reader.read_until(b'\n', &mut buffer).await?
        } else {
            buffer.resize(256 * 1024, 0);
            let n = reader.read(&mut buffer).await?;
            buffer.truncate(n);
            n
        };
        if n == 0 {
            break;
        }
        let chunk = if definer {
            strip_definer(&buffer)
        } else {
            std::borrow::Cow::Borrowed(buffer.as_slice())
        };
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        if last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            events(BackupEvent::Progress {
                bytes: written,
                total: None,
            });
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    events(BackupEvent::Progress {
        bytes: written,
        total: None,
    });
    Ok(written)
}

/// Feeds `path` to the tool's standard input, reporting bytes read of the file's size.
async fn feed_input(
    path: &Path,
    mut stdin: tokio::process::ChildStdin,
    events: EventSink<'_>,
    report: bool,
) -> std::io::Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    let total = file.metadata().await.ok().map(|m| m.len());
    let mut buffer = vec![0u8; 256 * 1024];
    let mut read = 0u64;
    let mut last = Instant::now();
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        stdin.write_all(&buffer[..n]).await?;
        read += n as u64;
        if report && last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            events(BackupEvent::Progress { bytes: read, total });
        }
    }
    stdin.shutdown().await?;
    drop(stdin);
    if report {
        events(BackupEvent::Progress { bytes: read, total });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_quote_what_needs_it() {
        let line = command_line(
            Path::new("/usr/bin/pg_dump"),
            &[
                "--format=c".into(),
                "--dbname=host='h' port='1'".into(),
                String::new(),
            ],
        );
        assert_eq!(
            line,
            r#"/usr/bin/pg_dump --format=c "--dbname=host='h' port='1'" """#
        );
    }
}
