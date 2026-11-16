//! What happens when the core, the app or the MCP server panics (SDB-163). A panic is a bug: it
//! is never swallowed. Where it happened goes to the app log ([`install_panic_log`]); the caller
//! that was waiting gets an error ([`catch_panic`], [`INTERNAL_ERROR`]) instead of an answer
//! that never comes, and everything else keeps working.
//!
//! A panic's message can hold a value or a secret (`expect` on a connection string, a cell):
//! the log keeps its place, and its message only through [`crate::audit::redact_secrets`] and
//! cut short. The error the user or the agent sees never holds it.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt as _;

use crate::AppLog;

/// What a caller is told when what it waited for panicked. The place of the panic is in the
/// app log; its message is never shown.
pub const INTERNAL_ERROR: &str =
    "an internal error stopped it (a bug in SpelaionDB): where it happened is in the app log";

/// The most of a panic's message the log keeps, after redaction.
const MESSAGE_MAX: usize = 300;

/// Runs `future`; if it panics, returns [`Panicked`] instead of unwinding into the caller. The
/// panic itself is reported by the panic hook ([`install_panic_log`]) before this returns.
pub async fn catch_panic<T>(future: impl Future<Output = T>) -> Result<T, Panicked> {
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|_| Panicked)
}

/// What [`catch_panic`] returns for a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{INTERNAL_ERROR}")]
pub struct Panicked;

/// Replaces the process's panic hook with one that writes where each panic happened to `log`
/// (and to standard error), its message redacted. Called once at the start of the app and of
/// `spelaiondb mcp`. The default hook is not kept: it would print the message as it is.
pub fn install_panic_log(log: Option<AppLog>) {
    std::panic::set_hook(Box::new(move |info| {
        let entry = panic_entry(info);
        eprintln!("{entry}");
        if let Some(log) = &log
            && let Err(e) = log.append(&entry)
        {
            eprintln!("could not write the log {}: {e}", log.path().display());
        }
    }));
}

/// The log entry for a panic: its place, its thread and its message, redacted and cut short.
fn panic_entry(info: &std::panic::PanicHookInfo<'_>) -> String {
    let place = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "an unknown place".into());
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("unnamed");
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_default();
    format!(
        "internal error (panic) at {place}, thread {thread}: {}",
        shorten(&crate::audit::redact_secrets(&message))
    )
}

/// `text` on one line, at most [`MESSAGE_MAX`] characters.
fn shorten(text: &str) -> String {
    let one_line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut chars = one_line.chars();
    let mut out: String = chars.by_ref().take(MESSAGE_MAX).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panic_becomes_an_error_and_a_normal_end_passes_through() {
        assert_eq!(catch_panic(async { 7 }).await, Ok(7));
        let caught = catch_panic(async {
            if std::hint::black_box(true) {
                panic!("the value postgres://ann:hunter2@db/shop");
            }
        })
        .await;
        assert_eq!(caught, Err(Panicked));
        assert!(!Panicked.to_string().contains("hunter2"));
    }

    #[test]
    fn the_log_entry_keeps_the_place_and_hides_secrets() {
        let entry = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = entry.clone();
        // Only this thread's panic is recorded: other tests may panic on purpose meanwhile.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().name() == Some("panics-worker") {
                *seen.lock().unwrap() = panic_entry(info);
            }
        }));
        let result = std::thread::Builder::new()
            .name("panics-worker".into())
            .spawn(|| panic!("could not connect to postgres://ann:hunter2@db/shop\nsecond line"))
            .unwrap()
            .join();
        std::panic::set_hook(previous);
        assert!(result.is_err());
        let entry = entry.lock().unwrap().clone();
        assert!(entry.contains("panics.rs:"), "{entry}");
        assert!(entry.contains("thread panics-worker"), "{entry}");
        assert!(entry.contains("could not connect"), "{entry}");
        assert!(!entry.contains("hunter2"), "{entry}");
        assert!(!entry.contains('\n'), "{entry}");
    }

    #[test]
    fn long_messages_are_cut_short() {
        let long = "x".repeat(MESSAGE_MAX * 2);
        let short = shorten(&long);
        assert_eq!(short.chars().count(), MESSAGE_MAX + 1);
        assert!(short.ends_with('…'));
    }
}
