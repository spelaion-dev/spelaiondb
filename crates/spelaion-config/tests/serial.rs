//! One test binary holds the files of `tests/` (SDB-168), and its files still run one after
//! another, as their own binaries did: a test takes its file's turn first. The tests of one file
//! share the turn and run in parallel as before; another file's tests wait until no test of the
//! file holding it is still running. Files keep their own names on the shared test servers and
//! in the temporary folder without having to know about each other, and the server-wide checks
//! (sessions, locks, timing of cancels) see only their own file's load.
//!
//! libtest starts tests in name order, so a file's tests mostly come together and the waits are
//! short, at the boundaries between files.

use std::sync::{Condvar, Mutex, PoisonError};

/// Which file holds the turn, and how many of its tests are running.
struct Turn {
    file: Option<&'static str>,
    running: usize,
}

static TURN: Mutex<Turn> = Mutex::new(Turn {
    file: None,
    running: 0,
});
static FREED: Condvar = Condvar::new();

/// Held by a running test; the turn passes on when the last test of its file ends (a failed test
/// drops it too, while it unwinds).
#[must_use = "the turn lasts as long as the value is held"]
pub struct FileTurn;

/// Waits for the turn of the file `module` (`module_path!()` of the test) is in, and takes it.
pub fn file_turn(module: &'static str) -> FileTurn {
    // "main::edit_source::statements" → "edit_source".
    let file = module.split("::").nth(1).unwrap_or(module);
    let mut turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    while turn.file.is_some_and(|holder| holder != file) {
        turn = FREED.wait(turn).unwrap_or_else(PoisonError::into_inner);
    }
    turn.file = Some(file);
    turn.running += 1;
    FileTurn
}

impl Drop for FileTurn {
    fn drop(&mut self) {
        let mut turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
        turn.running -= 1;
        if turn.running == 0 {
            turn.file = None;
            FREED.notify_all();
        }
    }
}
